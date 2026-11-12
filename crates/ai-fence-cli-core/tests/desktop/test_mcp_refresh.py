"""Owning-adapter MCP control; synthetic protocol only, no MCP leases."""
import json
import os
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import test_desktop as fixtures

desktop = fixtures.desktop


class McpRefreshTests(unittest.TestCase):
    setUp = fixtures.DesktopTests.setUp

    def connection(self):
        self.settings.update(release="loaded", mcp_protocol=dict(reload=True, thread_status=True))
        root = Path(self.settings["install_root"])
        root.mkdir()
        live = root / "launches" / (str(os.getpid()) + ".json")
        desktop.save(live, dict(format=1, **self.lease, profile=self.settings["profile"],
            state_home=self.settings["state_home"], mode="desktop-stdio", fenced=True, release="loaded"))
        session = desktop.ReconnectSession(self.settings, live, SimpleNamespace(threads={"owned_thread"}))
        session.client(dict(id=1, method="initialize"))
        session.native(dict(id=1, result={}))
        session.client(dict(method="initialized"))
        sent = []
        control = desktop.McpRefreshSession(self.settings, session, sent.append)
        return session, control, sent

    def schedule(self, **kwargs):
        return desktop.mcp_refresh(self.settings, pid=os.getpid(), thread_id="owned_thread", server_name="fixture_server", **kwargs)

    def report(self):
        return desktop.mcp_refresh(self.settings, status=True)

    def reply(self, control, sent, result=None, error=None):
        value = dict(id=sent[-1]["id"])
        value.update(error=error) if error is not None else value.update(result=result)
        self.assertTrue(control.native(value))

    def tools(self, arguments=("session_id", "session_secret"), **extra):
        return dict(data=[dict(name="fixture_server", tools={"resources.request":dict(inputSchema=dict(
            type="object", properties={name:dict(type="string", default="SECRET") for name in arguments},
            required=list(arguments)), description="SECRET")})], nextCursor=None, **extra)

    def test_reload_uses_owning_thread_and_consumes_internal_replies(self):
        session, control, sent = self.connection()
        self.schedule(expected={"resources.request":["session_id", "session_secret"]})
        control.poll()
        self.assertEqual(sent[-1]["method"], "mcpServerStatus/list")
        self.assertEqual(sent[-1]["params"]["threadId"], "owned_thread")
        self.reply(control, sent, result=self.tools(arguments=()))
        control.poll()
        self.assertEqual(sent[-1]["method"], "config/mcpServer/reload")
        self.reply(control, sent, result={})
        self.reply(control, sent, result=self.tools())
        report = self.report()
        self.assertEqual(report["status"], "verified")
        self.assertEqual(report["mission_control_session"], "not_checked")
        self.assertNotIn("SECRET", json.dumps(report))
        self.assertFalse(session.pending)
        self.assertFalse(control.native(dict(id="ordinary-client", result={})))
        count = len(sent)
        control.poll()
        self.assertEqual(len(sent), count)

    def test_active_inputs_turns_approvals_and_pending_requests_defer_reload(self):
        session, control, sent = self.connection()
        self.schedule()
        for name, busy in [("inputs", 1), ("active", {"owned_thread"}), ("approvals", {"approval"}),
                           ("pending", {"client":("config/read", None)})]:
            original = getattr(session, name)
            setattr(session, name, busy)
            control.poll()
            self.assertFalse(sent)
            setattr(session, name, original)
        control.poll()
        self.assertEqual(len(sent), 1)

    def test_new_turn_after_status_defers_the_reload_itself(self):
        session, control, sent = self.connection()
        self.schedule()
        control.poll()
        session.active.add("owned_thread")
        self.reply(control, sent, result=self.tools())
        control.poll()
        self.assertEqual(len(sent), 1)
        self.assertTrue(session.control_active)
        session.active.clear()
        control.poll()
        self.assertEqual(sent[-1]["method"], "config/mcpServer/reload")

    def test_wrong_thread_and_reused_process_identity_are_rejected(self):
        session, control, sent = self.connection()
        with self.assertRaisesRegex(RuntimeError, "not loaded"):
            desktop.mcp_refresh(self.settings, pid=os.getpid(), thread_id="other_thread", server_name="fixture_server")
        item = desktop.load(session.live)
        item["start"] = "old-process"
        desktop.save(session.live, item)
        with self.assertRaisesRegex(RuntimeError, "selected connection"):
            self.schedule()
        self.assertFalse(sent)

    def test_native_version_gate_and_unknown_status_method_never_attempt_reload(self):
        session, control, sent = self.connection()
        self.settings["mcp_protocol"] = {}
        self.schedule()
        control.poll()
        self.assertEqual(self.report()["status"], "unsupported")
        self.assertFalse(sent)
        self.settings["mcp_protocol"] = dict(reload=True, thread_status=True)
        self.schedule()
        control.poll()
        self.reply(control, sent, error=dict(code=-32601, message="SECRET"))
        self.assertEqual(self.report()["status"], "unsupported")
        self.assertEqual(len(sent), 1)
        self.assertNotIn("SECRET", json.dumps(self.report()))

    def test_timeout_clears_pending_and_consumes_late_reply(self):
        session, control, sent = self.connection()
        self.schedule()
        control.poll()
        identifier = sent[-1]["id"]
        control.request["expires_at"] = 0
        control.poll()
        self.assertEqual(self.report()["status"], "expired")
        self.assertFalse(session.pending)
        self.assertTrue(control.native(dict(id=identifier, result=self.tools())))
        self.assertEqual(self.report()["status"], "expired")

    def test_schema_arguments_are_checked_not_just_tool_names(self):
        session, control, sent = self.connection()
        self.schedule(inspect_only=True, expected={"resources.request":["session_id", "session_secret"]})
        control.poll()
        self.reply(control, sent, result=self.tools(arguments=("session_id",)))
        self.assertEqual(self.report()["status"], "schema_mismatch")
        self.assertEqual(self.report()["missing"], {"resources.request":["session_secret"]})
        self.assertEqual(len(sent), 1)

    def test_paginated_status_keeps_thread_scope_and_rejects_cursor_loop(self):
        session, control, sent = self.connection()
        self.schedule(inspect_only=True)
        control.poll()
        self.reply(control, sent, result=dict(data=[], nextCursor="next"))
        self.assertEqual(sent[-1]["params"], dict(threadId="owned_thread", serverName="fixture_server", limit=50, cursor="next"))
        self.reply(control, sent, result=dict(data=[], nextCursor="next"))
        self.assertEqual(self.report()["classification"], "native_status_invalid")
        self.assertFalse(session.pending)

    def test_failed_control_receipt_cannot_strand_or_disconnect_the_session(self):
        session, control, sent = self.connection()
        self.schedule()
        control.poll()
        original = desktop.load
        def load(path):
            if Path(path).name == "mcp-refresh-status.json":
                raise OSError("fixture receipt unavailable")
            return original(path)
        with patch.object(desktop, "load", side_effect=load):
            self.reply(control, sent, result=self.tools())
        self.assertIsNone(control.request)
        self.assertFalse(session.control_active)
        self.assertFalse(session.pending)
        self.assertEqual(len(sent), 1)

    def test_malformed_internal_reply_is_consumed_and_classified(self):
        session, control, sent = self.connection()
        self.schedule()
        control.poll()
        self.reply(control, sent, result=["invalid"])
        self.assertEqual(self.report()["classification"], "native_status_invalid")
        self.assertFalse(session.pending)


if __name__ == "__main__":
    unittest.main()
