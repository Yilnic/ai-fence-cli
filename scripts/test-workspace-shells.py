#!/usr/bin/env python3
"""Run generated shell helpers in real shells with a synthetic path-only CLI.

No real workspace, user profile, credential, agent or WSL process is changed.
On WSL, --windows enables Windows interop; on Windows it is the default.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def run(command, **kwargs):
    result = subprocess.run(command, capture_output=True, text=True, timeout=40, **kwargs)
    if result.returncode:
        raise AssertionError(f"{command[0]} failed ({result.returncode}): {result.stderr[-2000:]} {result.stdout[-1000:]}")
    return result.stdout.strip()


def ps_quote(value):
    return "'" + str(value).replace("'", "''") + "'"


def windows_path(path):
    if os.name == "nt":
        return str(path)
    return run(["wslpath", "-w", str(path)])


def executable_path(path):
    if os.name != "nt" and len(path) > 2 and path[1] == ":":
        return run(["wslpath", "-u", path])
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--helper", required=True, type=Path, help="ai-fence-workspace or complete ai-fence-cli")
    parser.add_argument("--windows", action="store_true", default=os.name == "nt")
    args = parser.parse_args()
    helper = [str(args.helper.resolve())]
    if args.helper.stem.startswith("ai-fence-cli"):
        helper.append("workspace")
    results = []
    with tempfile.TemporaryDirectory(prefix="af-workspace-shells-") as temporary:
        root = Path(temporary)
        target = root / "workspace with spaces & 日本語 literal"
        target.mkdir()
        (target / "fixture-marker").write_text("synthetic fixture")
        if args.windows:
            ps = shutil.which("powershell.exe") or "powershell.exe"
            commands = root / "tools 日本語"
            commands.mkdir()
            stub = commands / "fixture-cli.exe"
            source = commands / "fixture.cs"
            source.write_text('using System; using System.Text; public class Fixture { public static int Main(string[] a) {\n'
                'int i=0; if(a.Length>i && a[i]=="workspace") i++; if(a.Length>i && a[i]=="_wsl") i+=2;\n'
                'if(a.Length>i+1 && a[i]=="path") {if(a[i+1]=="missing") return 7; byte[] b=Encoding.UTF8.GetBytes('
                + json.dumps(windows_path(target) + "\n", ensure_ascii=False) + '); Console.OpenStandardOutput().Write(b,0,b.Length); return 0;}\n'
                'if(a.Length>i && a[i]=="fail") return 23; return 2; }}\n', encoding="utf-8-sig")
            run([ps,"-NoLogo","-NoProfile","-NonInteractive","-Command",
                "Add-Type -Path " + ps_quote(windows_path(source)) + " -OutputAssembly "
                + ps_quote(windows_path(stub)) + " -OutputType ConsoleApplication"])
            cli = windows_path(stub)
            # Exercise both Windows PowerShell 5.1 and installed PowerShell 7.
            ps7 = run([ps,"-NoProfile","-NonInteractive","-Command",
                "$p=Join-Path $env:ProgramFiles 'PowerShell/7/pwsh.exe'; if(Test-Path -LiteralPath $p){$p}"])
            for shell, executable in [("powershell", ps), ("powershell7", ps7)]:
                if not executable:
                    continue
                script = root / (shell + ".ps1")
                profile = root / (shell + "-profile.ps1")
                profile.write_text("$script:KeepFixture = 'λ 日本語'\n", encoding="utf-16")
                original = profile.read_bytes()
                command = [*helper, "shell-setup", "--shell", "powershell", "--cli", cli,
                    "--profile", str(profile), "--directory", str(root / (shell + " helpers 日本語"))]
                run(command)
                installed = profile.read_bytes()
                assert installed.startswith(original), "existing UTF-16 profile was changed"
                run(command)
                assert profile.read_bytes() == installed, "profile setup was not idempotent"
                generated = ". " + ps_quote(windows_path(profile)) + "\n"
                script.write_text(generated + "\nif ($script:KeepFixture -ne 'λ 日本語') {throw 'custom profile lost'}\n"
                    "$beforeEncoding=[Console]::OutputEncoding.CodePage\nfws ls\nif (-not (Test-Path -LiteralPath './fixture-marker')) {throw 'fws did not change directory'}\n"
                    "$before=(Get-Location).Path\nfws missing\nif ((Get-Location).Path -ne $before) {throw 'failed switch changed cwd'}\n"
                    "fw fail\nif ($LASTEXITCODE -ne 23) {throw 'fw lost exit status'}\n"
                    "if ([Console]::OutputEncoding.CodePage -ne $beforeEncoding) {throw 'console encoding changed'}\n", encoding="utf-8-sig")
                run([executable_path(executable),"-NoLogo","-NoProfile","-NonInteractive","-ExecutionPolicy","Bypass","-File",windows_path(script)])
                results.append(shell)
            cmd_dir=root / "cmd"
            run([*helper,"shell-setup","--shell","cmd","--cli",cli,"--directory",str(cmd_dir),"--no-profile"])
            check = root / "check.cmd"
            check.write_text('@echo off\nsetlocal DisableDelayedExpansion\nset "result=0"\n'
                'for /f "tokens=2 delims=:" %%C in (\'chcp\') do for /f %%N in ("%%C") do set "_test_cp=%%N"\n'
                'chcp 437 >nul\ncall "'
                + windows_path(cmd_dir / "fws.cmd") + '" ls\nif errorlevel 1 (set "result=1" & goto cleanup)\n'
                'if not exist fixture-marker (set "result=2" & goto cleanup)\ncall "'
                + windows_path(cmd_dir / "fws.cmd") + '" missing\nif not errorlevel 1 (set "result=3" & goto cleanup)\n'
                'if not exist fixture-marker (set "result=4" & goto cleanup)\ncall "'
                + windows_path(cmd_dir / "fw.cmd") + '" fail\nif not errorlevel 23 set "result=5"\n'
                'chcp | findstr "437" >nul\nif errorlevel 1 set "result=6"\n'
                ':cleanup\npopd\nif defined _test_cp chcp %_test_cp% >nul\nexit /b %result%\n')
            run([shutil.which("cmd.exe") or "cmd.exe","/d","/c",windows_path(check)])
            results.append("cmd")
            git_bash = run([ps,"-NoProfile","-NonInteractive","-Command",
                "$p=Join-Path $env:ProgramFiles 'Git/bin/bash.exe'; if(Test-Path -LiteralPath $p){$p}"])
            if git_bash:
                script = root / "git-bash.sh"
                generated=run([*helper,"shell-init","--shell","git-bash","--cli",cli])
                script.write_text(generated + "\nfws ls || exit\n[ -f fixture-marker ] || exit 21\n"
                    "before=$PWD\nfws missing && exit 22\n[ \"$PWD\" = \"$before\" ] || exit 24\n"
                    "fw fail\n[ \"$?\" -eq 23 ] || exit 25\n")
                run([executable_path(git_bash),"--noprofile","--norc",windows_path(script).replace("\\","/")])
                results.append("git-bash")
        if os.name != "nt":
            stub=root / "fixture-cli"
            stub.write_text("#!/bin/sh\n[ \"$1\" != workspace ] || shift\ncase \"$1\" in path) [ \"$2\" != missing ] || exit 7; printf '%s\\n' "
                + "'" + str(target).replace("'", "'\"'\"'") + "';; fail) exit 23;; *) exit 2;; esac\n")
            stub.chmod(0o700)
            for shell in ["bash","zsh"]:
                executable=shutil.which(shell)
                if not executable:
                    continue
                generated=run([*helper,"shell-init","--shell",shell,"--cli",str(stub)])
                script=root / (shell+".sh")
                script.write_text(generated+"\nfws ls || exit\n[ -f fixture-marker ] || exit 21\n"
                    "before=$PWD\nfws missing && exit 22\n[ \"$PWD\" = \"$before\" ] || exit 24\nfw fail\n[ \"$?\" -eq 23 ] || exit 25\n")
                run([executable,str(script)])
                results.append(shell)
    print(json.dumps({"passed":results,"real_workspaces_changed":False}))


if __name__ == "__main__":
    main()
