import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock

spec = importlib.util.spec_from_file_location("release", Path(__file__).resolve().parents[1] / "scripts/publish-release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class FakeGitHub:
    def __init__(self, existing):
        self.existing = existing
        self.writes = []

    def find_release(self, tag):
        return self.existing

    def request(self, method, path, value=None, content=None):
        if method != "GET":
            self.writes.append((method, path))
        raise AssertionError("no mutation permitted for an existing published release")


class ReleaseTests(unittest.TestCase):
    def plan(self):
        return {"repository": release.REPOSITORY, "version": "0.1.1", "artifacts": {
            target: {"filename": filename, "source_url": f"https://ai.yml.world/downloads/candidate/{filename}",
                     "sha256": "a" * 64, "size_bytes": 123}
            for target, filename in release.TARGETS.items()}}

    def test_all_manifests_pin_their_version_and_platform(self):
        plan = self.plan()
        self.assertEqual(release.validate_plan(plan), "v0.1.1")
        for target, filename in release.TARGETS.items():
            manifest = release.manifest(plan, target)
            self.assertEqual(manifest["download_url"], f"https://github.com/Yilnic/ai-fence-cli/releases/download/v0.1.1/{filename}")
            self.assertEqual(manifest["target"], target)
            self.assertNotIn("/latest/", manifest["download_url"])

    def test_invalid_or_partial_plans_are_rejected(self):
        mutations = [lambda p: p.update(version="../0.1.1"),
                     lambda p: p.update(repository="other/repo"),
                     lambda p: p["artifacts"].pop("macos-aarch64"),
                     lambda p: p["artifacts"]["linux-x86_64"].update(filename="../binary"),
                     lambda p: p["artifacts"]["linux-x86_64"].update(source_url="https://example.test/binary"),
                     lambda p: p["artifacts"]["linux-x86_64"].update(sha256=""),
                     lambda p: p["artifacts"]["linux-x86_64"].update(size_bytes=-1)]
        for mutate in mutations:
            plan = self.plan()
            mutate(plan)
            with self.assertRaises(ValueError):
                release.validate_plan(plan)

    def test_existing_release_is_immutable_even_if_incomplete(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "binary"
            path.write_bytes(b"approved binary")
            for assets in [[], [{"name": "binary", "digest": "sha256:" + "0" * 64}]]:
                api = FakeGitHub({"draft": False, "assets": assets})
                with self.assertRaises(ValueError):
                    release.publish(api, "v0.1.1", [path], "notes")
                self.assertEqual(api.writes, [])

    def test_identical_published_release_is_a_noop(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "binary"
            path.write_bytes(b"approved binary")
            api = FakeGitHub({"draft": False, "assets": [{"name": path.name, "digest": "sha256:" + release.digest(path)}]})
            release.publish(api, "v0.1.1", [path], "notes")
            self.assertEqual(api.writes, [])

    def test_mismatching_draft_is_not_overwritten(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "binary"
            path.write_bytes(b"approved binary")
            api = FakeGitHub({"draft": True, "assets": [{"name": path.name, "digest": "sha256:" + "0" * 64}]})
            with self.assertRaises(ValueError):
                release.publish(api, "v0.1.1", [path], "notes")
            self.assertEqual(api.writes, [])

    def test_draft_is_published_only_after_uploaded_assets_are_verified(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "binary"
            path.write_bytes(b"approved binary")
            asset = {"name": path.name, "digest": "sha256:" + release.digest(path)}
            api = Mock()
            api.find_release.return_value = None
            api.request.side_effect = [
                {"id": 7, "draft": True, "assets": []}, asset,
                {"id": 7, "draft": True, "assets": [asset]}, {},
            ]
            release.publish(api, "v0.1.1", [path], "reviewed notes")
            calls = api.request.call_args_list
            self.assertTrue(calls[0].args[2]["draft"])
            self.assertEqual(calls[1].kwargs["content"], path.read_bytes())
            self.assertEqual(calls[2].args, ("GET", "/releases/7"))
            self.assertEqual(calls[3].args[:2], ("PATCH", "/releases/7"))
            self.assertFalse(calls[3].args[2]["draft"])

    def test_bad_upload_digest_leaves_draft_unpublished(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "binary"
            path.write_bytes(b"approved binary")
            api = Mock()
            api.find_release.return_value = {"id": 7, "draft": True, "assets": []}
            api.request.return_value = {"name": path.name, "digest": "sha256:" + "0" * 64}
            with self.assertRaises(ValueError):
                release.publish(api, "v0.1.1", [path], "notes")
            self.assertEqual(api.request.call_count, 1)
            self.assertEqual(api.request.call_args.args[0], "POST")


if __name__ == "__main__":
    unittest.main()
