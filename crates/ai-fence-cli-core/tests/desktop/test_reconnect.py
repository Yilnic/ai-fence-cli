"""Cooperative reconnects: no production signals, history or provider requests."""
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import test_desktop as fixtures

desktop = fixtures.desktop


class ReconnectTests(unittest.TestCase):
    setUp = fixtures.DesktopTests.setUp

    def session(self):
        root = Path(self.settings["install_root"])
        self.settings.update(release="new", cli_sha256="fixture", files={})
        root.mkdir(exist_ok=True)
        desktop.save(root / "settings.json", self.settings)
        live = root / "launches" / (str(os.getpid()) + ".json")
        desktop.save(live, dict(format=1, **self.lease, mode="desktop-stdio", fenced=True,
                               profile=self.settings["profile"], state_home=self.settings["state_home"],
                               release="old", cli_sha256="old"))
        lifecycle = SimpleNamespace(threads={"thread_a", "thread_b"})
        session = desktop.ReconnectSession(self.settings, live, lifecycle)
        session.client(dict(id=1, method="initialize"))
        session.native(dict(id=1, result={}))
        session.client(dict(method="initialized"))
        return root, live, session

    def schedule(self, **kwargs):
        return desktop.reconnect(self.settings, apply=True, **kwargs)

    def test_preview_never_arms_and_apply_is_idempotent(self):
        root, live, session = self.session()
        preview = desktop.reconnect(self.settings)
        self.assertEqual(len(preview["targets"]), 1)
        self.assertFalse((root / "reconnect-request.json").exists())
        first, second = self.schedule(), self.schedule()
        self.assertEqual(first["operation_id"], second["operation_id"])
        self.assertFalse(session.draining)

    def test_turns_pending_requests_and_approvals_block_until_idle(self):
        root, live, session = self.session()
        session.client(dict(id=2, method="turn/start", params={"threadId": "thread_a", "input": "SECRET"}))
        session.native(dict(id=2, result={"turn": {"id": "turn_a", "status": "inProgress"}}))
        self.schedule()
        self.assertFalse(session.poll())
        session.native(dict(id="approval", method="item/commandExecution/requestApproval", params={"command": "SECRET"}))
        session.native(dict(method="turn/completed", params={"threadId": "thread_a", "turn": {"id": "turn_a"}}))
        self.assertFalse(session.poll())
        session.client(dict(id="approval", result={"decision": "accept"}))
        session.client(dict(id=3, method="config/read"))
        self.assertFalse(session.poll())
        session.native(dict(id=3, result={}))
        self.assertTrue(session.poll())
        self.assertTrue(session.draining)
        with self.assertRaisesRegex(RuntimeError, "reconnect"):
            session.client(dict(id=4, method="turn/start"))
        session.finish()
        report = desktop.reconnect(self.settings, status=True)
        self.assertEqual(report["reopen_thread_ids"], ["thread_a", "thread_b"])
        self.assertEqual(report["targets"][0]["status"], "closed")
        for path in root.rglob("*.json"):
            self.assertNotIn("SECRET", path.read_text())

    def test_failed_turn_start_does_not_keep_a_phantom_active_turn(self):
        root, live, session = self.session()
        session.client(dict(id=2, method="turn/start", params={"threadId": "thread_a"}))
        session.native(dict(id=2, error={"code": -32602}))
        self.schedule()
        self.assertTrue(session.poll())

    def test_completion_before_start_reply_remains_idle(self):
        root, live, session = self.session()
        session.client(dict(id=2, method="turn/start", params={"threadId": "thread_a"}))
        session.native(dict(method="turn/completed", params={"threadId": "thread_a", "turn": {"id": "turn_a"}}))
        session.native(dict(id=2, result={"turn": {"id": "turn_a", "status": "inProgress"}}))
        self.schedule()
        self.assertTrue(session.poll())

    def test_cancel_expiry_and_concurrent_setup_never_stop_a_session(self):
        for reason in ["cancel", "expiry", "setup"]:
            with self.subTest(reason=reason):
                root, live, session = self.session()
                self.schedule()
                if reason == "cancel":
                    desktop.reconnect(self.settings, cancel=True)
                elif reason == "expiry":
                    request = desktop.load(root / "reconnect-request.json")
                    request["expires_at"] = 0
                    desktop.save(root / "reconnect-request.json", request)
                else:
                    desktop.save(root / "settings.json", dict(self.settings, release="another"))
                self.assertFalse(session.poll())
                self.assertFalse(session.draining)
                # Retire this fixture's operation before the next subtest.
                desktop.reconnect(self.settings, cancel=True)

    def test_legacy_reused_pid_and_other_profile_are_never_selected_for_shutdown(self):
        root, live, session = self.session()
        receipt = desktop.load(live)
        receipt.pop("reconnect_control")
        desktop.save(live, receipt)
        result = self.schedule()
        self.assertEqual(result["status"], "manual_reconnect_required")
        self.assertFalse((root / "reconnect-request.json").exists())
        receipt.update(start="another-boot:1", reconnect_control=1)
        desktop.save(live, receipt)
        self.assertFalse(self.schedule()["targets"])
        receipt.update(start=self.lease["start"], profile="other-account")
        desktop.save(live, receipt)
        self.assertFalse(self.schedule()["targets"])

    def test_malformed_request_and_modified_release_leave_connection_running(self):
        root, live, session = self.session()
        self.schedule()
        (root / "reconnect-request.json").write_text('{"invalid": true}')
        self.assertFalse(session.poll())
        self.assertFalse(session.draining)
        self.assertEqual(desktop.load(live)["reconnect_state"]["error"], "reconnect_request_rejected")
        (root / "reconnect-request.json").unlink()
        payload = root / "fixture-payload"
        payload.write_text("changed")
        self.settings["files"] = {"fixture-payload": "0" * 64}
        desktop.save(root / "settings.json", self.settings)
        with self.assertRaisesRegex(RuntimeError, "integrity"):
            self.schedule()
        self.assertFalse((root / "reconnect-request.json").exists())

    def test_starting_connection_waits_for_initialization(self):
        root, live, session = self.session()
        session.ready = False
        self.schedule()
        self.assertFalse(session.poll())
        session.client(dict(method="initialized"))
        self.assertTrue(session.poll())

    def test_update_failure_and_check_only_never_schedule_reconnect(self):
        root, live, session = self.session()
        arguments = ["--install-root", str(root), "update"]
        with contextlib.redirect_stdout(io.StringIO()), patch.object(desktop, "wait_for_payloads"), patch.object(desktop.updates, "check", return_value={"status": "failed"}), patch.object(desktop, "reconnect") as schedule:
            self.assertEqual(desktop.control([*arguments, "--reconnect"]), 1)
            schedule.assert_not_called()
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
            desktop.control([*arguments, "--check-only", "--reconnect"])
        self.assertEqual(error.exception.code, 2)
        self.assertFalse((root / "reconnect-request.json").exists())

    def test_update_reconnects_by_default_and_explicit_disable_wins(self):
        root, live, session = self.session()
        arguments = ["--install-root", str(root), "update"]
        for flags, environment, expected in [([], {}, True), ([], {"AI_FENCE_CODEX_RECONNECT": "off"}, False),
                                              (["--no-reconnect"], {}, False),
                                              (["--reconnect"], {"AI_FENCE_CODEX_RECONNECT": "off"}, True)]:
            with self.subTest(flags=flags, environment=environment), patch.dict(os.environ, environment), contextlib.redirect_stdout(io.StringIO()), patch.object(desktop, "wait_for_payloads"), patch.object(desktop.updates, "check", return_value={"status": "current"}), patch.object(desktop, "reconnect", return_value={"status": "scheduled"}) as schedule:
                self.assertEqual(desktop.control([*arguments, *flags]), 0)
                self.assertEqual(schedule.called, expected)

    def test_session_environment_vetoes_automatic_but_not_explicit_reconnect(self):
        root, live, session = self.session()
        desktop.reconnect(self.settings, apply=True, automatic=True)
        with patch.dict(os.environ, {"AI_FENCE_CODEX_RECONNECT": "off"}):
            self.assertFalse(session.poll())
            self.assertFalse(session.draining)
        desktop.reconnect(self.settings, apply=True)
        with patch.dict(os.environ, {"AI_FENCE_CODEX_RECONNECT": "off"}):
            self.assertTrue(session.poll())

    def test_auth_wait_keeps_native_output_readable_and_prevents_idle_shutdown(self):
        root, live, session = self.session()
        self.schedule()
        acquired = threading.Event()
        def native_reply():
            with session.gate:
                session.native(dict(id="auth-response", result={}))
                acquired.set()
        # Authentication can synchronously await a reply handled by the output
        # thread. Its input activity must count as busy without holding the gate.
        with session.input():
            reader = threading.Thread(target=native_reply)
            reader.start()
            self.assertTrue(acquired.wait(2), "native output blocked behind authentication input")
            reader.join(timeout=2)
            self.assertFalse(session.poll())
        self.assertTrue(session.poll())

    def test_failed_control_receipt_does_not_strand_a_draining_connection(self):
        root, live, session = self.session()
        self.schedule()
        with patch.object(session, "mark", side_effect=OSError("fixture write failure")):
            self.assertFalse(session.poll())
        self.assertFalse(session.draining)
        session.client(dict(id=99, method="config/read"))
        session.native(dict(id=99, result={}))
        self.assertTrue(session.poll())


@unittest.skipUnless(os.environ.get("AF_TEST_CLI") and os.environ.get("AF_TEST_NATIVE"), "set AF_TEST_CLI and AF_TEST_NATIVE")
class NativeReconnect(unittest.TestCase):
    def test_official_command_closes_only_selected_connection_and_resumes_same_thread(self):
        import tempfile
        from test_native import Client
        with tempfile.TemporaryDirectory(prefix="af-native-reconnect-") as temporary:
            root = Path(temporary)
            state, home, project = root / "state", root / "home", root / "project"
            for path in [state, home, project]:
                path.mkdir()
            dirty = project / "untracked.txt"
            dirty.write_text("preserve work")
            auth = '{"auth_mode":"apikey","OPENAI_API_KEY":"fixture-invalid-no-inference"}'
            config = 'model="fixture-model"\n'
            (state / "auth.json").write_text(auth)
            (state / "config.toml").write_text(config)
            environment = {k: v for k, v in os.environ.items() if not k.startswith(("AI_FENCE_", "OPENAI_", "ANTHROPIC_", "CODEX_")) and not k.upper().endswith("_PROXY")}
            environment.update(HOME=str(home), XDG_CONFIG_HOME=str(root / "config"), AI_FENCE_HOME=str(root / "fence"),
                               AI_FENCE_SNAPSHOTS="off", OTEL_SDK_DISABLED="true")
            cli, native = str(Path(os.environ["AF_TEST_CLI"]).resolve()), str(Path(os.environ["AF_TEST_NATIVE"]).resolve())
            install = root / "install"
            command = [cli, "codex-desktop", "--install-root", str(install)]
            subprocess.run([*command, "setup", "--native-codex", native, "--state-home", str(state),
                            "--auth-home", str(state), "--profile", "fixture", "--updates", "off"],
                           env=environment, capture_output=True, timeout=30, check=True)
            clients = []
            def control(*args):
                result = subprocess.run([*command, *args], env=environment, capture_output=True, timeout=15)
                self.assertEqual(result.returncode, 0, "official reconnect command failed")
                return json.loads(result.stdout)
            try:
                first, peer = Client(install / "bin/codex", environment), Client(install / "bin/codex", environment)
                clients += [first, peer]
                peer_endpoint = peer.endpoint(2)
                started = first.request(2, "thread/start", dict(cwd=str(project)))
                thread_id = started["thread"]["id"]
                # An empty native thread has no durable rollout yet. Persist
                # synthetic fixture history through the API, with no model turn.
                first.request(3, "thread/inject_items", dict(threadId=thread_id, items=[{
                    "type": "message", "role": "user", "content": [
                        {"type": "input_text", "text": "Synthetic reconnect fixture; no inference."}]
                }]))
                preview = control("reconnect", "--pid", str(first.process.pid))
                self.assertEqual(len(preview["targets"]), 1)
                self.assertIn(thread_id, preview["targets"][0]["threads"])
                self.assertFalse((install / "reconnect-request.json").exists())
                scheduled = control("reconnect", "--apply", "--pid", str(first.process.pid))
                self.assertEqual(scheduled["status"], "scheduled")
                first.process.wait(timeout=12)
                first.close()
                clients.remove(first)
                self.assertEqual(peer.endpoint(3), peer_endpoint)
                closed = control("reconnect", "--status")
                self.assertEqual(closed["status"], "closed")
                self.assertEqual(closed["reopen_thread_ids"], [thread_id])
                resumed = Client(install / "bin/codex", environment)
                clients.append(resumed)
                result = resumed.request(2, "thread/resume", dict(threadId=thread_id))
                self.assertEqual(result["thread"]["id"], thread_id)
                self.assertEqual(result["thread"]["cwd"], str(project))
                replacements = control("reconnect", "--status")["replacement_launches"]
                self.assertTrue(any(v["pid"] == resumed.process.pid and v["verified"] for v in replacements))
                self.assertEqual(dirty.read_text(), "preserve work")
                self.assertEqual((state / "auth.json").read_text(), auth)
                self.assertEqual((state / "config.toml").read_text(), config)
            finally:
                for client in clients:
                    client.close(terminate=True)


if __name__ == "__main__":
    unittest.main()
