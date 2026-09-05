#!/usr/bin/env python3
"""Publish verified, versioned CLI assets without replacing released files."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
from urllib.error import HTTPError
from urllib.request import Request, urlopen

REPOSITORY = "Yilnic/ai-fence-cli"
TARGETS = {
    "linux-x86_64": "ai-fence-cli-linux-x86_64",
    "windows-x86_64": "ai-fence-cli-windows-x86_64.exe",
    "macos-aarch64": "ai-fence-cli-macos-aarch64",
}


def validate_plan(plan):
    version = plan["version"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("invalid release version")
    if plan["repository"] != REPOSITORY or set(plan["artifacts"]) != set(TARGETS):
        raise ValueError("release must contain all three supported targets for the canonical repository")
    for target, filename in TARGETS.items():
        artifact = plan["artifacts"][target]
        if artifact["filename"] != filename:
            raise ValueError("unexpected artifact filename")
        if artifact["source_url"] != f"https://ai.yml.world/downloads/candidate/{filename}":
            raise ValueError("artifact source is not the production candidate endpoint")
        if not re.fullmatch(r"[0-9a-f]{64}", artifact["sha256"]):
            raise ValueError("invalid SHA-256")
        if type(artifact["size_bytes"]) is not int or not 0 < artifact["size_bytes"] < 2_000_000_000:
            raise ValueError("invalid artifact size")
    return f"v{version}"


def manifest(plan, target):
    artifact = plan["artifacts"][target]
    return {
        "cli_name": "ai-fence-cli", "version": plan["version"], "target": target,
        "filename": artifact["filename"], "size_bytes": artifact["size_bytes"],
        "sha256": artifact["sha256"],
        "download_url": f"https://github.com/{REPOSITORY}/releases/download/v{plan['version']}/{artifact['filename']}",
    }


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def stage(plan, destination):
    validate_plan(plan)
    destination.mkdir(parents=True, exist_ok=True)
    files = []
    for target, filename in TARGETS.items():
        artifact = plan["artifacts"][target]
        path = destination / filename
        request = Request(artifact["source_url"], headers={"User-Agent": "Mozilla/5.0"})
        with urlopen(request, timeout=120) as response, path.open("wb") as output:
            size = 0
            while chunk := response.read(1024 * 1024):
                size += len(chunk)
                if size > artifact["size_bytes"]:
                    raise ValueError(f"{filename}: download exceeds the approved size")
                output.write(chunk)
        if size != artifact["size_bytes"] or digest(path) != artifact["sha256"]:
            raise ValueError(f"{filename}: candidate changed; prepare and validate a new release plan")
        if target == "linux-x86_64":
            path.chmod(0o700)
            # Do not expose the publication credential to the executable.
            result = subprocess.run([str(path.resolve()), "--version"], env={"PATH": "/usr/bin:/bin"},
                                    capture_output=True, text=True, timeout=15, check=True)
            if result.stdout.strip() != f"ai-fence-cli {plan['version']}":
                raise ValueError("Linux binary version does not match the release")
        manifest_path = destination / f"ai-fence-cli-manifest-{target}.json"
        manifest_path.write_text(json.dumps(manifest(plan, target), separators=(",", ":")) + "\n")
        files.extend([path, manifest_path])
    checksums = destination / "SHA256SUMS"
    checksums.write_text("".join(f"{digest(path)}  {path.name}\n" for path in sorted(files)))
    return files + [checksums]


class GitHub:
    def __init__(self, token):
        self.token = token

    def request(self, method, path, value=None, content=None):
        host = "https://uploads.github.com" if content is not None else "https://api.github.com"
        data = content if content is not None else None if value is None else json.dumps(value).encode()
        headers = {"Authorization": f"Bearer {self.token}", "Accept": "application/vnd.github+json",
                   "User-Agent": "AI-Fence-Release", "X-GitHub-Api-Version": "2022-11-28",
                   "Content-Type": "application/octet-stream" if content is not None else "application/json"}
        with urlopen(Request(host + f"/repos/{REPOSITORY}" + path, data=data, headers=headers, method=method), timeout=180) as response:
            return json.load(response)

    def find_release(self, tag):
        try:
            return self.request("GET", f"/releases/tags/{tag}")
        except HTTPError as error:
            if error.code == 404:
                return None
            raise


def publish(api, tag, files, notes):
    expected = {path.name: "sha256:" + digest(path) for path in files}
    release = api.find_release(tag)
    if release is None:
        release = api.request("POST", "/releases", {
            "tag_name": tag, "name": f"AI Fence CLI {tag} Preview", "body": notes,
            "draft": True, "prerelease": True, "make_latest": "false",
        })
    existing = {asset["name"]: asset for asset in release.get("assets", [])}
    if len(existing) != len(release.get("assets", [])) or set(existing) - set(expected):
        raise ValueError("unexpected existing release assets; refusing to modify this release")
    for name, asset in existing.items():
        if asset.get("digest") != expected[name]:
            raise ValueError(f"{name}: existing asset differs; published versions must never be overwritten")
    if not release["draft"]:
        if set(existing) != set(expected):
            raise ValueError("published release is incomplete; publish a new version instead")
        print(f"{tag} already published with identical assets; no changes")
        return
    for path in files:
        if path.name not in existing:
            asset = api.request("POST", f"/releases/{release['id']}/assets?name={path.name}", content=path.read_bytes())
            if asset.get("digest") != expected[path.name]:
                raise ValueError(f"{path.name}: uploaded asset digest mismatch; leaving draft unpublished")
    checked = api.request("GET", f"/releases/{release['id']}")
    if {asset["name"]: asset.get("digest") for asset in checked["assets"]} != expected:
        raise ValueError("release asset verification failed; leaving draft unpublished")
    api.request("PATCH", f"/releases/{release['id']}", {
        "body": notes, "draft": False, "prerelease": True, "make_latest": "false",
    })
    print(f"Published https://github.com/{REPOSITORY}/releases/tag/{tag}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("plan", type=Path)
    parser.add_argument("--stage-only", type=Path, help="verify and stage assets locally without publishing")
    args = parser.parse_args()
    plan = json.loads(args.plan.read_text())
    tag = validate_plan(plan)
    if os.environ.get("GITHUB_REF_TYPE") and (os.environ["GITHUB_REF_TYPE"] != "tag" or os.environ.get("GITHUB_REF_NAME") != tag):
        raise ValueError("workflow ref must be the exact approved release tag")
    if args.stage_only:
        files = stage(plan, args.stage_only)
        print(f"Verified {len(files)} assets in {args.stage_only}")
        return
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if not token:
        raise ValueError("GitHub publication requires GH_TOKEN or the Actions GITHUB_TOKEN")
    notes = args.plan.with_suffix(".md").read_text()
    with tempfile.TemporaryDirectory(prefix="ai-fence-release-") as tmp:
        files = stage(plan, Path(tmp))
        publish(GitHub(token), tag, files, notes)


if __name__ == "__main__":
    main()
