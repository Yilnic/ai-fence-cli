"""Shared named workspace protocol: no user state, model turns or live installs."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SOURCE = Path(__file__).resolve().parents[2] / "assets/codex_desktop.py"
spec = importlib.util.spec_from_file_location("named_desktop", SOURCE)
desktop = importlib.util.module_from_spec(spec)
spec.loader.exec_module(desktop)


class NamedWorkspaceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="af-named-desktop-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / "project"
        self.workspace = self.root / "workspaces/feature-a"
        self.home = self.root / "native-state"
        self.registry = self.root / "registry"
        for path in [self.source, self.workspace, self.home, self.registry / "records"]:
            path.mkdir(parents=True)
        self.settings = dict(format=1, profile="personal", state_home=str(self.home),
                             snapshot_policy="off", roots=[], install_root=str(self.root / "install"),
                             workspace_registry=str(self.registry))
        self.lease = dict(pid=os.getpid(), start=desktop.process_identity(os.getpid()))
        self.value = dict(id="workspace-a", name="feature-a", registry=str(self.registry),
                          project=self.rust_identity(self.source), workspace=self.rust_identity(self.workspace),
                          lease="lease-a", owned=True)
        self.calls = []
        self.saved = None

    def rust_identity(self, path):
        result = desktop.identity(path)
        result["path"] = result.pop("physical")
        return result

    def bridge(self, settings, action, *args, **kwargs):
        self.calls.append((action, args, kwargs))
        if action == "_attach":
            value = dict(self.value)
            value["owned"] = sum(item[0] == "_attach" for item in self.calls) == 1
            return value
        if action == "_resolve":
            return self.saved
        if action == "_release":
            return dict(released=True)
        self.fail("unexpected workspace bridge action")

    def lifecycle(self):
        self.context = patch.object(desktop, "named_context", return_value=dict(id="workspace-a"))
        self.context.start(); self.addCleanup(self.context.stop)
        self.bridge_patch = patch.object(desktop, "named_call", side_effect=self.bridge)
        self.bridge_patch.start(); self.addCleanup(self.bridge_patch.stop)
        return desktop.Lifecycle(self.settings, self.lease)

    def start(self, lifecycle, thread_id="thread-a", request_id=1):
        lifecycle.request(dict(id=request_id, method="thread/start", params=dict(cwd=str(self.workspace))))
        self.assertTrue(any(call[0] == "_attach" for call in self.calls), "lease must exist before forwarding thread/start")
        lifecycle.response(dict(id=request_id, result=dict(thread=dict(id=thread_id, cwd=str(self.workspace)))))

    def released(self):
        return [call for call in self.calls if call[0] == "_release"]

    def test_desktop_registers_original_project_and_native_store_without_copying_history(self):
        (self.home / "auth.json").write_text("fixture-auth")
        (self.home / "history.jsonl").write_text("fixture-history")
        lifecycle = self.lifecycle()
        self.start(lifecycle)
        record = desktop.load(desktop.record_path(self.settings, "thread-a"))
        self.assertEqual(record["mode"], "named")
        self.assertEqual(record["project"], desktop.identity(self.source))
        self.assertEqual(record["workspace"], desktop.identity(self.workspace))
        self.assertEqual(record["named_workspace"]["id"], "workspace-a")
        self.assertEqual((self.home / "auth.json").read_text(), "fixture-auth")
        self.assertEqual((self.home / "history.jsonl").read_text(), "fixture-history")
        lifecycle.close()
        self.assertEqual(len(self.released()), 1)

    def test_unsubscribe_does_not_unlock_a_thread_before_native_close(self):
        lifecycle = self.lifecycle()
        self.start(lifecycle)
        lifecycle.request(dict(id=2, method="thread/unsubscribe", params=dict(threadId="thread-a")))
        lifecycle.response(dict(id=2, result=dict(status="unsubscribed")))
        self.assertFalse(self.released())
        lifecycle.response(dict(method="thread/closed", params=dict(threadId="thread-a")))
        self.assertEqual(len(self.released()), 1)
        self.assertEqual(desktop.load(desktop.record_path(self.settings, "thread-a"))["leases"], [])
        lifecycle.close()
        self.assertEqual(len(self.released()), 1)

    def test_last_thread_controls_shared_workspace_lease_release(self):
        lifecycle = self.lifecycle()
        self.start(lifecycle, "thread-a", 1)
        self.start(lifecycle, "thread-b", 2)
        lifecycle.response(dict(method="thread/closed", params=dict(threadId="thread-a")))
        self.assertFalse(self.released())
        lifecycle.response(dict(method="thread/status/changed", params=dict(threadId="thread-b", status=dict(type="notLoaded"))))
        self.assertEqual(len(self.released()), 1)

    def test_failed_thread_start_releases_its_uncommitted_reservation(self):
        lifecycle = self.lifecycle()
        lifecycle.request(dict(id=1, method="thread/start", params=dict(cwd=str(self.workspace))))
        lifecycle.response(dict(id=1, error=dict(code=-32602, message="fixture native validation")))
        self.assertEqual(len(self.released()), 1)
        self.assertFalse(desktop.record_path(self.settings, "thread-a").exists())

    def test_failed_resume_releases_provisional_thread_and_workspace_leases(self):
        lifecycle = self.lifecycle()
        self.saved = dict(record=dict(id="workspace-a"), cwd=self.rust_identity(self.workspace))
        record = desktop.attach(self.settings, "thread-a", self.workspace, self.lease, named=self.value)
        record["leases"] = []
        path = desktop.record_path(self.settings, "thread-a")
        desktop.save(path, record)
        lifecycle.request(dict(id=1, method="thread/resume", params=dict(threadId="thread-a")))
        self.assertEqual(desktop.load(path)["leases"], [self.lease])
        lifecycle.response(dict(id=1, error=dict(code=-32600, message="fixture native resume rejected")))
        self.assertEqual(desktop.load(path)["leases"], [])
        self.assertEqual(len(self.released()), 1)

    def test_busy_workspace_does_not_leave_a_local_thread_lease(self):
        lifecycle = self.lifecycle()
        self.saved = dict(record=dict(id="workspace-a"), cwd=self.rust_identity(self.workspace))
        record = desktop.attach(self.settings, "thread-a", self.workspace, self.lease, named=self.value)
        record["leases"] = []
        path = desktop.record_path(self.settings, "thread-a")
        desktop.save(path, record)
        def busy(settings, action, *args, **kwargs):
            if action == "_attach":
                raise RuntimeError("workspace active in another launch")
            return self.bridge(settings, action, *args, **kwargs)
        with patch.object(desktop, "named_call", side_effect=busy):
            with self.assertRaisesRegex(RuntimeError, "active"):
                lifecycle.request(dict(id=1, method="thread/resume", params=dict(threadId="thread-a")))
        self.assertEqual(desktop.load(path)["leases"], [])

    def test_failed_request_on_loaded_thread_keeps_its_existing_workspace(self):
        lifecycle = self.lifecycle()
        self.start(lifecycle)
        self.saved = dict(record=dict(id="workspace-a"), cwd=self.rust_identity(self.workspace))
        lifecycle.request(dict(id=3, method="thread/resume", params=dict(threadId="thread-a")))
        lifecycle.response(dict(id=3, error=dict(code=-32602, message="fixture invalid resume option")))
        self.assertFalse(self.released())
        self.assertEqual(desktop.load(desktop.record_path(self.settings, "thread-a"))["leases"], [self.lease])
        lifecycle.close()

    def test_client_opt_out_is_preserved_while_internal_close_is_observed(self):
        lifecycle = self.lifecycle()
        request = dict(id=0, method="initialize", params=dict(capabilities=dict(
            optOutNotificationMethods=["thread/closed", "thread/status/changed", "item/agentMessage/delta"])))
        lifecycle.request(request)
        self.assertEqual(request["params"]["capabilities"]["optOutNotificationMethods"], ["item/agentMessage/delta"])
        self.start(lifecycle)
        forward = lifecycle.response(dict(method="thread/closed", params=dict(threadId="thread-a")))
        self.assertFalse(forward)
        self.assertEqual(len(self.released()), 1)

    def test_resume_reuses_saved_subdirectory_without_checkpoint_restore(self):
        directory = self.workspace / "src"
        directory.mkdir()
        (directory / "untracked").write_text("keep new work")
        self.saved = dict(record=dict(id="workspace-a"), cwd=self.rust_identity(directory))
        with patch.object(desktop, "named_call", side_effect=self.bridge):
            for _ in range(2):
                parameters = dict(threadId="thread-a")
                desktop.preflight_resume(self.settings, parameters)
                self.assertEqual(parameters["cwd"], str(directory))
        self.assertEqual((directory / "untracked").read_text(), "keep new work")
        self.assertTrue(all(call[0] == "_resolve" for call in self.calls))

    def test_missing_workspace_fails_resume_without_history_changes_or_fallback(self):
        (self.home / "native.sqlite").write_bytes(b"fixture unchanged")
        parameters = dict(threadId="thread-a")
        with patch.object(desktop, "named_call", side_effect=RuntimeError("saved workspace missing")):
            with self.assertRaisesRegex(RuntimeError, "missing"):
                desktop.preflight_resume(self.settings, parameters)
        self.assertNotIn("cwd", parameters)
        self.assertEqual((self.home / "native.sqlite").read_bytes(), b"fixture unchanged")

    def test_resume_never_silently_changes_workspace(self):
        self.saved = dict(record=dict(id="workspace-a"), cwd=self.rust_identity(self.workspace))
        with patch.object(desktop, "named_call", side_effect=self.bridge), patch.object(desktop, "named_context", return_value=dict(id="workspace-b")):
            with self.assertRaisesRegex(RuntimeError, "another named workspace"):
                desktop.preflight_resume(self.settings, dict(threadId="thread-a", cwd=str(self.source)))

    def test_native_cli_history_is_located_through_metadata_only(self):
        self.settings["native_codex"] = "/fixture/native-codex"
        with patch.object(desktop, "named_call", side_effect=self.bridge), patch.object(desktop, "native_thread", return_value=dict(cwd=str(self.workspace))) as native, patch.object(desktop, "named_context", return_value=dict(id="workspace-a")):
            parameters = dict(threadId="cli-thread")
            desktop.preflight_resume(self.settings, parameters)
        self.assertEqual(parameters["cwd"], str(self.workspace))
        native.assert_called_once_with(self.settings, "cli-thread")

    def test_native_cwd_flags_are_respected_without_changing_forwarded_arguments(self):
        arguments = ["--yolo", "--cd", str(self.workspace), "resume", "--all"]
        before = list(arguments)
        self.assertEqual(desktop.native_cwd(arguments), self.workspace)
        self.assertEqual(arguments, before)
        self.assertEqual(desktop.native_cwd(["--", "--cd", "/prompt-text"]), Path.cwd())


if __name__ == "__main__":
    unittest.main()
