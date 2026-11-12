"""Windows scheduled-task supervisor for a verified, explicitly installed proxy."""
import argparse
import hashlib
import json
import logging
import msvcrt
import os
from pathlib import Path
import subprocess
import time


def save(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2), encoding="utf-8")
    os.replace(temporary, path)


def supervise(root):
    with (root / "proxy-service.lock").open("a+b") as lock:
        lock.seek(0)
        lock.write(b"0")
        lock.flush()
        lock.seek(0)
        try:
            msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
        except OSError:
            raise RuntimeError("A proxy supervisor already owns this installation") from None
        settings = json.loads((root / "proxy-settings.json").read_text(encoding="utf-8-sig"))
        manifest = json.loads((root / "release-manifest.json").read_text(encoding="utf-8-sig"))
        binary = root / manifest["filename"]
        if hashlib.sha256(binary.read_bytes()).hexdigest() != manifest["sha256"]:
            raise RuntimeError("Windows proxy release checksum mismatch")
        channel = settings.get("channel", "dev")  # Existing custom DEV installations.
        if channel not in {"dev", "prod"}:
            raise RuntimeError("Unsupported proxy channel")
        environment = os.environ.copy()
        environment["AI_FENCE_CLI_CHANNEL"] = channel
        for name in ("AI_FENCE_DEV_KEY", "AI_FENCE_DEV_API_KEY", "AI_FENCE_KEY", "AI_FENCE_API_KEY"):
            environment.pop(name, None)
        key_name = "AI_FENCE_DEV_API_KEY" if channel == "dev" else "AI_FENCE_API_KEY"
        environment[key_name] = (root / "api-key").read_text(encoding="utf-8-sig").strip()
        command = [str(binary), "proxy", "--fence-url", settings["fence_url"],
                   "--port", str(settings["port"]), "--subscription-mode"]
        logging.info("supervisor started pid=%s", os.getpid())
        while not (root / "stop-proxy").exists():
            with (root / "proxy.stdout.log").open("ab") as stdout, (root / "proxy.stderr.log").open("ab") as stderr:
                child = subprocess.Popen(command, env=environment, stdin=subprocess.DEVNULL,
                                         stdout=stdout, stderr=stderr,
                                         creationflags=subprocess.CREATE_NO_WINDOW)
                save(root / "proxy-state.json", dict(manager_pid=os.getpid(), proxy_pid=child.pid,
                     started_at=time.time(), channel=channel, fence_url=settings["fence_url"],
                     local_url=settings["local_url"], binary_sha256=manifest["sha256"]))
                logging.info("proxy started pid=%s", child.pid)
                while child.poll() is None and not (root / "stop-proxy").exists():
                    time.sleep(1)
                if child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()
                logging.info("proxy exited pid=%s code=%s", child.pid, child.returncode)
            if (root / "stop-proxy").exists():
                break
            time.sleep(3)
        logging.info("supervisor stopped by stop-proxy request")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path)
    args = parser.parse_args()
    root = args.root.resolve(strict=True)
    logging.basicConfig(filename=root / "service.stderr.log", level=logging.INFO,
                        format="%(asctime)s %(levelname)s %(message)s")
    try:
        supervise(root)
    except Exception:
        logging.exception("proxy supervisor failed")
        raise SystemExit(1) from None


if __name__ == "__main__":
    main()
