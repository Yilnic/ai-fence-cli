"""Real bundled CLI + native Codex, synthetic workspace registry, no model turns."""
import json
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import uuid

from test_native import Client
from test_named_workspaces import desktop


@unittest.skipUnless(os.environ.get("AF_TEST_CLI") and os.environ.get("AF_TEST_NATIVE"), "set AF_TEST_CLI and AF_TEST_NATIVE")
class NativeNamedWorkspaceIntegration(unittest.TestCase):
    def test_shared_engine_leases_parallel_desktop_threads_resume_and_manual_cli(self):
        with tempfile.TemporaryDirectory(prefix="af-native-named-") as directory:
            root = Path(directory)
            home, state, project, registry = [root / name for name in ["home", "state", "project", "registry"]]
            for path in [home, state, project, registry / "records"]:
                path.mkdir(parents=True)
            auth = json.dumps({"auth_mode":"apikey", "OPENAI_API_KEY":"fixture-invalid-no-inference"})
            config = 'model="fixture-model"\n'
            (state / "auth.json").write_text(auth)
            (state / "config.toml").write_text(config)
            environment = dict(PATH=os.environ.get("PATH", os.defpath), HOME=str(home), LANG="C.UTF-8",
                XDG_CONFIG_HOME=str(root / "config"), XDG_DATA_HOME=str(root / "data"), XDG_CACHE_HOME=str(root / "cache"),
                AI_FENCE_HOME=str(root / "fence"), AI_FENCE_WORKSPACE_HOME=str(registry),
                AI_FENCE_CLI_AUTO_UPDATE="never", OTEL_SDK_DISABLED="true")
            def identity(path):
                value = desktop.identity(path)
                value["path"] = value.pop("physical")
                return value
            workspaces = {}
            for name in ["alpha", "beta", "gamma"]:
                path = root / "workspaces" / name
                path.mkdir(parents=True)
                (path / "untracked").write_text("keep fixture work")
                identifier = str(uuid.uuid4())
                record = dict(format="ai-fence-named-workspace-v1", id=identifier, name=name,
                    project=identity(project), logical_root=str(project), workspace=identity(path),
                    created_at="2026-10-17T00:00:00Z", backend="btrfs", base_head=None, pinned=False, deleted=False,
                    chats=[], native_homes=[], checkpoints=[], lease=None)
                (registry / "records" / (identifier + ".json")).write_text(json.dumps(record))
                (path / ".ai-fence-workspace.json").write_text(json.dumps(dict(format=record["format"], id=identifier, registry=str(registry))))
                workspaces[name] = (path, identifier)
            cli = str(Path(os.environ["AF_TEST_CLI"]).resolve())
            install = root / "adapter"
            setup = [cli, "codex-desktop", "--install-root", str(install), "setup", "--native-codex",
                str(Path(os.environ["AF_TEST_NATIVE"]).resolve()), "--state-home", str(state), "--auth-home", str(state),
                "--profile", "fixture-native", "--updates", "off", "--workspace-registry", str(registry)]
            subprocess.run(setup, env=environment, capture_output=True, check=True, timeout=20)
            settings = json.loads((install / "settings.json").read_text())
            self.assertEqual(hashlib.sha256((install / "releases" / settings["release"] / "runtime.py").read_bytes()).hexdigest(),
                             hashlib.sha256(Path(desktop.__file__).read_bytes()).hexdigest())
            self.assertEqual(settings["snapshot_policy"], "off")
            def current(name):
                return json.loads((registry / "records" / (workspaces[name][1] + ".json")).read_text())
            clients = []
            try:
                first = Client(install / "bin/codex", environment)
                clients.append(first)
                started = first.request(2, "thread/start", dict(cwd=str(workspaces["alpha"][0])))
                thread_id = started["thread"]["id"]
                # Native empty threads may not have a resumable rollout yet.
                # Persist synthetic history through the supported API without
                # starting a model turn or editing any transcript file.
                first.request(3, "thread/inject_items", dict(threadId=thread_id, items=[{
                    "type":"message", "role":"user", "content":[{"type":"input_text", "text":"Synthetic workspace resume fixture; no inference."}]
                }]))
                self.assertEqual(current("alpha")["chats"][0]["thread_id"], thread_id)
                self.assertEqual(current("alpha")["chats"][0]["state_home"], str(state))
                second = Client(install / "bin/codex", environment)
                clients.append(second)
                with self.assertRaisesRegex(AssertionError, "code=-32602"):
                    second.request(2, "thread/start", dict(cwd=str(workspaces["alpha"][0])))
                self.assertEqual(second.last_error["data"], dict(origin="ai_fence", stage="workspace_preflight", retryable=False, native_forwarded=False))
                # Local validation leaves the same stdio connection usable.
                second.request(3, "thread/start", dict(cwd=str(workspaces["beta"][0])))
                self.assertNotEqual(first.endpoint(4), second.endpoint(4))
                denied = subprocess.run([cli, "ws", "run", "alpha", "--", "true"], cwd=project,
                    env=environment, capture_output=True, timeout=10)
                self.assertNotEqual(denied.returncode, 0)
                self.assertIn(b"active in another launch", denied.stderr)
                first.close()
                clients.remove(first)
                free = subprocess.run([cli, "ws", "run", "alpha", "--", "true"], cwd=project,
                    env=environment, capture_output=True, timeout=10)
                self.assertEqual(free.returncode, 0, free.stderr.decode(errors="replace")[:500])
                resumed = second.request(5, "thread/resume", dict(threadId=thread_id))
                self.assertEqual(resumed["thread"]["cwd"], str(workspaces["alpha"][0]))
                forked = second.request(6, "thread/fork", dict(threadId=thread_id, cwd=str(workspaces["gamma"][0])))
                self.assertNotEqual(forked["thread"]["id"], thread_id)
                self.assertEqual(forked["thread"]["cwd"], str(workspaces["gamma"][0]))
                self.assertEqual(current("gamma")["chats"][0]["thread_id"], forked["thread"]["id"])
                self.assertEqual((workspaces["alpha"][0] / "untracked").read_text(), "keep fixture work")
                second.close(terminate=True)
                clients.remove(second)
                self.assertEqual((state / "auth.json").read_text(), auth)
                self.assertEqual((state / "config.toml").read_text(), config)
                self.assertFalse(list((install / "launches").glob("*.json")))
                # Install the actual official alias in a temporary HOME. A
                # synthetic native child keeps this manual-cd guard test
                # independent of provider/model availability.
                commands = root / "manual-bin"
                native_directory = root / "fixture-native"
                native_directory.mkdir()
                native = native_directory / "codex"
                native.write_text("#!" + sys.executable + '\nimport json,os,sys\n'
                    'if "--version" in sys.argv:\n print("codex-cli fixture");sys.exit(0)\n'
                    'if "app-server" not in sys.argv: sys.exit(0)\n'
                    'for line in sys.stdin:\n'
                    ' value=json.loads(line)\n'
                    ' if "id" not in value: continue\n'
                    ' result={}\n'
                    ' if value.get("method")=="config/read": result={"config":{"model_providers":{"ai_fence":{"base_url":os.environ["OPENAI_BASE_URL"]}}}}\n'
                    ' print(json.dumps({"id":value["id"],"result":result}),flush=True)\n')
                native.chmod(0o700)
                subprocess.run([cli, "install-self", "--install-dir", str(commands), "--data-dir", str(root / "manual-data"),
                    "--no-completions", "--quiet"], env=environment, capture_output=True, check=True, timeout=20)
                manual_environment = dict(environment, PATH=str(native_directory) + os.pathsep + environment["PATH"],
                    AI_FENCE_FENCED_MODE="standalone", AI_FENCE_CODEX_STANDALONE_AUTH="api", OPENAI_API_KEY="fixture-invalid-no-inference")
                manual = Client(commands / "codex-fenced", manual_environment, cwd=workspaces["alpha"][0])
                clients.append(manual)
                self.assertEqual(current("alpha")["lease"]["owner"]["pid"], manual.process.pid)
                self.assertTrue(current("alpha")["native_homes"])
                blocked = subprocess.run([cli, "ws", "run", "alpha", "--", "true"], cwd=project,
                    env=environment, capture_output=True, timeout=10)
                self.assertNotEqual(blocked.returncode, 0)
                manual.close()
                clients.remove(manual)
                self.assertIsNone(current("alpha")["lease"])
            finally:
                for client in clients:
                    client.close(terminate=True)


if __name__ == "__main__":
    unittest.main()
