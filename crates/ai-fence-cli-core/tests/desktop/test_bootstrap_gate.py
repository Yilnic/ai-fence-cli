"""Candidate assets and synthetic native processes; never real credentials."""
import json
import contextlib
import io
import os
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from unittest.mock import patch

import test_desktop as fixtures

desktop = fixtures.desktop


class BootstrapGateTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="af-bootstrap-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        release = self.root / "releases" / "candidate"
        release.mkdir(parents=True)
        for source, target in [("codex_desktop.py", "runtime.py"), ("codex_desktop_auth.py", "codex_desktop_auth.py"),
                               ("codex_desktop_updates.py", "codex_desktop_updates.py")]:
            shutil.copyfile(fixtures.SOURCE.with_name(source), release / target)
        # Synthetic CLI transport delegates to the actual candidate runtime.
        # It never binds a listener or contacts a provider.
        cli = release / "ai-fence-cli"
        cli.write_text("#!" + sys.executable + "\n" + '''
import os, sys
os.environ['OPENAI_BASE_URL'] = 'http://127.0.0.1:1/v1'
arguments = sys.argv[sys.argv.index('--') + 1:]
os.execv(arguments[0], arguments)
''')
        cli.chmod(0o700)
        helper = release / "codex-runtime"
        helper.write_text("#!" + sys.executable + "\nimport os,sys\nos.execv(sys.executable, [sys.executable, " +
                          repr(str(release / "runtime.py")) + ", 'runtime', *sys.argv[1:]])\n")
        helper.chmod(0o700)
        owner = self.root / "owner"
        owner.mkdir()
        (owner / "auth.json").write_text(json.dumps(dict(tokens=dict(access_token="unit-access", account_id="unit-account"))))
        native = self.root / "native"
        native.write_text("#!" + sys.executable + "\n" + '''
import json, os, sys
owner = os.environ['CODEX_HOME'].endswith('/owner')
initialized = False
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    if method == 'initialized':
        initialized = True
        continue
    if 'id' not in request:
        continue
    if method == 'getAuthStatus':
        assert request['params'] == dict(includeToken=False, refreshToken=False)
        result = dict(authMethod='chatgpt' if owner else None, requiresOpenaiAuth=True)
    elif method == 'account/read':
        assert initialized
        assert request['params'] == dict(refreshToken=False)
        result = dict(account=dict(type='chatgpt') if owner else None)
    else:
        assert method in {'initialize', 'account/login/start'}
        result = {}
    print(json.dumps(dict(id=request['id'], result=result)), flush=True)
''')
        native.chmod(0o700)
        state = self.root / "real-history"
        state.mkdir()
        (state / "marker").write_text("keep")
        self.settings = dict(install_root=str(self.root), release="candidate", native_codex=str(native),
                             native_version="codex-cli fixture", state_home=str(state), state_identity=desktop.identity(state),
                             profile="unit", auth_mode="native-owner", auth_home=str(owner), launch_prefix=[],
                             files={}, format=1, cli_sha256="unit")

    def test_real_candidate_bridge_passes_exact_desktop_bootstrap_without_touching_history(self):
        report = desktop.updates.bootstrap_check(self.settings)
        self.assertEqual(report["status"], "ok", report)
        self.assertEqual(list(Path(self.settings["state_home"]).iterdir()), [Path(self.settings["state_home"]) / "marker"])
        self.assertNotIn("unit-access", json.dumps(report))
        self.assertNotIn("unit-account", json.dumps(report))

    def test_previous_broken_asset_fails_legacy_bootstrap(self):
        auth = self.root / "releases/candidate/codex_desktop_auth.py"
        auth.write_text(auth.read_text().replace(' and method != "getAuthStatus"', ''))
        report = desktop.updates.bootstrap_check(self.settings)
        self.assertEqual(report["classification"], "owner_auth_not_confirmed", report)

    def test_explicit_null_auth_token_passes_desktop_bootstrap(self):
        native = Path(self.settings["native_codex"])
        native.write_text(native.read_text().replace(
            "result = dict(authMethod=", "result = dict(authToken=None, authMethod="))
        report = desktop.updates.bootstrap_check(self.settings)
        self.assertEqual(report["status"], "ok", report)

    def test_returned_auth_token_still_rejects_candidate(self):
        native = Path(self.settings["native_codex"])
        native.write_text(native.read_text().replace(
            "result = dict(authMethod=", "result = dict(authToken='unit-unexpected-token', authMethod="))
        report = desktop.updates.bootstrap_check(self.settings)
        self.assertEqual(report["classification"], "owner_auth_not_confirmed", report)
        self.assertNotIn("unit-unexpected-token", json.dumps(report))

    def test_admission_delay_is_not_a_sign_in_failure(self):
        self.settings["launch_prefix"] = [sys.executable, "-c", "import time;time.sleep(10)"]
        report = desktop.updates.bootstrap_check(self.settings, timeout=0.05)
        self.assertEqual(report["classification"], "startup_or_admission_timeout", report)

    def test_failed_candidate_cannot_switch_or_schedule_reconnect(self):
        # Use the updater fixture to exercise its activation boundary, not an
        # isolated assertion about the preflight helper.
        from test_desktop_updates import UpdateTests
        fixture = UpdateTests()
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        _, fetch, run, _ = fixture.fixture()
        selected = Path(fixture.settings["install_root"]) / "settings.json"
        before = selected.read_bytes()
        failed = dict(status="failed", classification="protocol_rejected", stage="getAuthStatus")
        with patch.object(desktop.updates, "bootstrap_check", return_value=failed):
            result = desktop.updates.check(fixture.settings, force=True, opener=fetch, run=run)
        self.assertEqual(result["failure_stage"], "bootstrap")
        self.assertEqual(selected.read_bytes(), before)
        self.assertFalse((selected.parent / "previous-settings.json").exists())
        self.assertFalse((selected.parent / "reconnect-request.json").exists())

    def test_manual_upgrade_failure_preserves_router_settings_and_previous_release(self):
        fixture = fixtures.DesktopTests()
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        original, args = fixture.install()
        paths = [args.install_root / name for name in original["files"] if not name.startswith("releases/")]
        paths += [args.install_root / "settings.json", args.install_root / "snapshot-roots.json"]
        before = {path:path.read_bytes() for path in paths}
        admission = fixture.root / "admission"
        admission.write_text('#!/bin/sh\nexec "$@"\n')
        admission.chmod(0o700)
        args.launch_prefix = [str(admission)]
        failed = dict(status="failed", classification="protocol_timeout", stage="getAuthStatus")
        with patch.dict(os.environ, {"AI_FENCE_DESKTOP_CLI_SOURCE":str(fixture.root / "fence-cli")}), \
                patch.object(desktop.updates, "bootstrap_check", return_value=failed), \
                contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, "previous release remains selected"):
                desktop.install(args)
        self.assertEqual({path:path.read_bytes() for path in paths}, before)
        self.assertFalse((args.install_root / "previous-settings.json").exists())


if __name__ == "__main__":
    unittest.main()
