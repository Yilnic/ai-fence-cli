//! Shell navigation runs in the caller; Windows management runs in WSL.
use super::*;
use clap::ValueEnum;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum WorkspaceShell {
    Bash,
    Zsh,
    GitBash,
    Powershell,
    Cmd,
}

pub fn detected() -> WorkspaceShell {
    if std::env::var_os("MSYSTEM").is_some() {
        WorkspaceShell::GitBash
    } else if std::env::var_os("PSModulePath").is_some() {
        WorkspaceShell::Powershell
    } else if cfg!(windows) {
        WorkspaceShell::Cmd
    } else if std::env::var("SHELL").is_ok_and(|value| value.ends_with("/zsh")) {
        WorkspaceShell::Zsh
    } else {
        WorkspaceShell::Bash
    }
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn names(channel: Option<&str>) -> Result<(String, String)> {
    if let Some(value) = channel {
        anyhow::ensure!(
            !value.is_empty()
                && value
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)),
            "invalid shell helper channel"
        );
    }
    let suffix = channel.map(|value| format!("-{value}")).unwrap_or_default();
    Ok((format!("fw{suffix}"), format!("fws{suffix}")))
}

pub fn render(
    shell: WorkspaceShell,
    cli: &Path,
    suffix: &[OsString],
    channel: Option<&str>,
) -> Result<String> {
    let (fw, fws) = names(channel)?;
    let windows = cfg!(windows) || matches!(shell, WorkspaceShell::GitBash | WorkspaceShell::Cmd);
    let mut arguments = vec![cli.as_os_str().to_owned()];
    arguments.extend_from_slice(suffix);
    if windows {
        arguments.extend(["_wsl".into(), "--".into()]);
    }
    let text: Vec<_> = arguments.iter().map(|v| v.to_string_lossy()).collect();
    let prefix = text
        .iter()
        .map(|v| {
            sh_quote(&if windows {
                v.replace('\\', "/")
            } else {
                v.to_string()
            })
        })
        .collect::<Vec<_>>()
        .join(" ");
    match shell {
        WorkspaceShell::Bash | WorkspaceShell::Zsh | WorkspaceShell::GitBash => Ok(format!(
            r#"# AI Fence: fw = Fence workspace; fws = Fence workspace switch.
{fw}() {{ {msys}{prefix} "$@"; }}
{fws}() {{
    [ "$#" -gt 0 ] || {{ printf '%s\n' 'Usage: {fws} NAME [--project PATH]' >&2; return 2; }}
    local _fws_path
    _fws_path=$({fw} path "$@"{path_flag}) || return
    [ -n "$_fws_path" ] || return 1
    {convert}
    builtin cd -- "$_fws_path"
}}
if [ -n "${{BASH_VERSION-}}" ]; then
    _ai_fence_fws_complete() {{
        local _fws_names
        _fws_names=$({fw} ls --names 2>/dev/null)
        COMPREPLY=( $(compgen -W "$_fws_names" -- "${{COMP_WORDS[COMP_CWORD]}}") )
    }}
    complete -F _ai_fence_fws_complete {fws}
fi
"#,
            msys = if windows {
                "MSYS2_ARG_CONV_EXCL='*' "
            } else {
                ""
            },
            path_flag = if windows { " --windows" } else { "" },
            convert = if windows {
                "_fws_path=${_fws_path%$'\\r'}; _fws_path=$(cygpath -u -- \"$_fws_path\") || return"
            } else {
                ":"
            }
        )),
        WorkspaceShell::Powershell => {
            let prefix = text
                .iter()
                .map(|v| ps_quote(v))
                .collect::<Vec<_>>()
                .join(" ");
            Ok(format!(
                r#"# AI Fence: fw = Fence workspace; fws = Fence workspace switch.
# PowerShell's built-in fw alias is Format-Wide; workspace initialization owns this shortcut.
if (Test-Path 'Alias:{fw}') {{ Remove-Item -LiteralPath 'Alias:{fw}' -Force }}
function global:{fw} {{
    $savedEncoding = [Console]::OutputEncoding
    $code = 1
    try {{
        [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding
        & {prefix} @args
        $code = $LASTEXITCODE
    }} finally {{ [Console]::OutputEncoding = $savedEncoding }}
    $global:LASTEXITCODE = $code
}}
function global:{fws} {{
    if ($args.Count -eq 0) {{ throw 'Usage: {fws} NAME [--project PATH]' }}
    $target = @({fw} 'path' @args {path_flag})
    if ($LASTEXITCODE -ne 0) {{ return }}
    if ($target.Count -ne 1 -or [string]::IsNullOrWhiteSpace($target[0])) {{ throw 'Invalid workspace path response' }}
    Set-Location -LiteralPath $target[0]
}}
"#,
                path_flag = if windows { "'--windows'" } else { "" }
            ))
        }
        WorkspaceShell::Cmd => {
            anyhow::ensure!(
                text.iter().all(|v| !v.contains(['"', '%', '\r', '\n'])),
                "CMD helper executable path contains unsupported characters"
            );
            let prefix = text
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(" ");
            Ok(format!(
                r#"@echo off
rem AI Fence workspace navigation. pushd supports WSL UNC paths; popd returns.
if "%~1"=="" (echo Usage: {fws} NAME [--project PATH] 1>&2 & exit /b 2)
setlocal DisableDelayedExpansion
for /f "tokens=2 delims=:" %%C in ('chcp') do for /f %%N in ("%%C") do set "_fws_cp=%%N"
if not defined _fws_cp exit /b 1
chcp 65001 >nul
set "_fws_path="
for /f "usebackq delims=" %%P in (`"{prefix} path %* --windows"`) do set "_fws_path=%%P"
if not defined _fws_path (chcp %_fws_cp% >nul & exit /b 1)
endlocal & chcp %_fws_cp% >nul & pushd "%_fws_path%"
"#
            )
            .replace('\n', "\r\n"))
        }
    }
}

/// Update only our delimited profile block; preserve all other bytes.
fn initialize_profile(profile: &Path, loader: &str, channel: Option<&str>) -> Result<()> {
    let tag = channel.unwrap_or("prod");
    let begin = format!("# >>> AI Fence workspace {tag} >>>");
    let end = format!("# <<< AI Fence workspace {tag} <<<");
    let original = match fs::read(profile) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let (encoding, existing) =
        if original.starts_with(&[0xff, 0xfe]) || original.starts_with(&[0xfe, 0xff]) {
            anyhow::ensure!(
                original.len().is_multiple_of(2),
                "invalid UTF-16 shell profile; original preserved"
            );
            let little = original[0] == 0xff;
            let units: Vec<_> = original[2..]
                .chunks_exact(2)
                .map(|part| {
                    if little {
                        u16::from_le_bytes([part[0], part[1]])
                    } else {
                        u16::from_be_bytes([part[0], part[1]])
                    }
                })
                .collect();
            (
                if little { 2 } else { 3 },
                String::from_utf16(&units)
                    .context("invalid UTF-16 shell profile; original preserved")?,
            )
        } else {
            let bom = original.starts_with(&[0xef, 0xbb, 0xbf]);
            let encoding = if bom
                || (original.is_empty() && profile.extension().is_some_and(|ext| ext == "ps1"))
            {
                1
            } else {
                0
            };
            (
                encoding,
                String::from_utf8(original[if bom { 3 } else { 0 }..].to_vec())
                    .context("unsupported shell profile encoding; original preserved")?,
            )
        };
    let block = format!("{begin}\n{loader}\n{end}");
    let updated = if let Some(start) = existing.find(&begin) {
        let finish = existing[start..]
            .find(&end)
            .context("incomplete AI Fence shell profile block; inspect before repair")?
            + start
            + end.len();
        format!("{}{}{}", &existing[..start], block, &existing[finish..])
    } else {
        format!(
            "{}{block}\n",
            if existing.is_empty() || existing.ends_with('\n') {
                existing
            } else {
                existing + "\n"
            }
        )
    };
    if let Some(parent) = profile.parent() {
        fs::create_dir_all(parent)?;
    }
    // Follow a user's dotfile symlink instead of replacing the symlink itself.
    let encoded = match encoding {
        2 | 3 => {
            let mut bytes = if encoding == 2 {
                vec![0xff, 0xfe]
            } else {
                vec![0xfe, 0xff]
            };
            bytes.extend(updated.encode_utf16().flat_map(|unit| {
                if encoding == 2 {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                }
            }));
            bytes
        }
        1 => {
            let mut bytes = vec![0xef, 0xbb, 0xbf];
            bytes.extend(updated.as_bytes());
            bytes
        }
        _ => updated.into_bytes(),
    };
    fs::write(profile, encoded)
        .with_context(|| format!("cannot initialize shell profile {}", profile.display()))
}

pub fn setup(
    shell: WorkspaceShell,
    cli: &Path,
    suffix: &[OsString],
    directory: Option<&Path>,
    profile: Option<&Path>,
    initialize: bool,
) -> Result<serde_json::Value> {
    let directory = directory.map(PathBuf::from).unwrap_or(
        dirs::data_local_dir()
            .context("cannot locate shell helper directory")?
            .join("ai-fence/workspace-shells"),
    );
    install(shell, cli, suffix, &directory, profile, initialize, None)
}

fn install(
    shell: WorkspaceShell,
    cli: &Path,
    suffix: &[OsString],
    directory: &Path,
    profile: Option<&Path>,
    initialize: bool,
    channel: Option<&str>,
) -> Result<serde_json::Value> {
    fs::create_dir_all(directory)?;
    let (fw, fws) = names(channel)?;
    let extension = match shell {
        WorkspaceShell::Powershell => "ps1",
        WorkspaceShell::Cmd => "cmd",
        _ => "sh",
    };
    let helper = directory.join(format!("{fws}.{extension}"));
    let rendered = render(shell, cli, suffix, channel)?;
    if shell == WorkspaceShell::Powershell {
        let mut bytes = vec![0xef, 0xbb, 0xbf];
        bytes.extend(rendered.as_bytes());
        fs::write(&helper, bytes)?;
    } else {
        fs::write(&helper, rendered)?;
    }
    let mut profiles = Vec::new();
    if shell == WorkspaceShell::Cmd {
        let mut prefix = format!("\"{}\"", cli.display());
        for arg in suffix {
            prefix.push_str(&format!(" \"{}\"", arg.to_string_lossy()));
        }
        fs::write(
            directory.join(format!("{fw}.cmd")),
            format!(
                r#"@echo off
setlocal DisableDelayedExpansion
for /f "tokens=2 delims=:" %%C in ('chcp') do for /f %%N in ("%%C") do set "_fw_cp=%%N"
if not defined _fw_cp exit /b 1
chcp 65001 >nul
{prefix} _wsl -- %*
set "_fw_status=%errorlevel%"
chcp %_fw_cp% >nul
exit /b %_fw_status%
"#
            )
            .replace('\n', "\r\n"),
        )?;
        #[cfg(windows)]
        if initialize {
            initialize_user_path(directory)?;
        }
    } else if initialize {
        let home = dirs::home_dir().context("cannot locate shell profile")?;
        let default = match shell {
            WorkspaceShell::Zsh => std::env::var_os("ZDOTDIR")
                .map(PathBuf::from)
                .unwrap_or(home.clone())
                .join(".zshrc"),
            WorkspaceShell::Powershell if cfg!(windows) => dirs::document_dir()
                .unwrap_or_else(|| home.join("Documents"))
                .join("WindowsPowerShell/profile.ps1"),
            WorkspaceShell::Powershell => std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"))
                .join("powershell/profile.ps1"),
            _ => home.join(".bashrc"),
        };
        let profile = profile.unwrap_or(&default);
        let loader = if shell == WorkspaceShell::Powershell {
            if helper.to_string_lossy().is_ascii() {
                format!(". {}", ps_quote(&helper.to_string_lossy()))
            } else {
                // Keep an existing ASCII/ANSI profile valid in PowerShell 5.1.
                let units = helper
                    .to_string_lossy()
                    .encode_utf16()
                    .map(|u| format!("0x{u:04x}"))
                    .collect::<Vec<_>>()
                    .join(",");
                format!(". (-join [char[]]@({units}))")
            }
        } else {
            format!(
                ". {}",
                sh_quote(&helper.to_string_lossy().replace('\\', "/"))
            )
        };
        initialize_profile(profile, &loader, channel)?;
        profiles.push(profile.to_path_buf());
        if shell == WorkspaceShell::GitBash && profile == default {
            let login = home.join(".bash_profile");
            initialize_profile(&login, &loader, channel)?;
            profiles.push(login);
        }
    }
    let restart_shell = !profiles.is_empty();
    Ok(
        serde_json::json!({"helper":helper,"profiles":profiles,"management":fw,"switch":fws,"restart_shell":restart_shell,"cmd_add_to_path":if shell==WorkspaceShell::Cmd {Some(directory)}else{None}}),
    )
}

/// Called by official setup/install-self. No background process is installed.
pub fn install_automatic(
    cli: &Path,
    suffix: &[OsString],
    data: &Path,
    bin: &Path,
    channel: Option<&str>,
    initialize: bool,
) -> Result<()> {
    let home = dirs::home_dir().context("cannot locate shell profiles")?;
    let directory = data.join("workspace-shells");
    let bash = if cfg!(windows) {
        WorkspaceShell::GitBash
    } else {
        WorkspaceShell::Bash
    };
    install(
        bash,
        cli,
        suffix,
        &directory.join("bash"),
        None,
        initialize,
        channel,
    )?;
    if !cfg!(windows) && (home.join(".zshrc").exists() || detected() == WorkspaceShell::Zsh) {
        install(
            WorkspaceShell::Zsh,
            cli,
            suffix,
            &directory.join("zsh"),
            None,
            initialize,
            channel,
        )?;
    }
    install(
        WorkspaceShell::Powershell,
        cli,
        suffix,
        &directory.join("powershell"),
        None,
        initialize
            && (cfg!(windows)
                || detected() == WorkspaceShell::Powershell
                || home.join(".config/powershell").exists()),
        channel,
    )?;
    if cfg!(windows) {
        install(
            WorkspaceShell::Cmd,
            cli,
            suffix,
            bin,
            None,
            initialize,
            channel,
        )?;
        let ps7 = dirs::document_dir()
            .unwrap_or_else(|| home.join("Documents"))
            .join("PowerShell/profile.ps1");
        if initialize {
            install(
                WorkspaceShell::Powershell,
                cli,
                suffix,
                &directory.join("powershell"),
                Some(&ps7),
                true,
                channel,
            )?;
        }
    } else {
        let (fw, _) = names(channel)?;
        let mut command = sh_quote(&cli.to_string_lossy());
        for arg in suffix {
            command.push(' ');
            command.push_str(&sh_quote(&arg.to_string_lossy()));
        }
        let target = bin.join(fw);
        fs::write(&target, format!("#!/bin/sh\nexec {command} \"$@\"\n"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn initialize_user_path(directory: &Path) -> Result<()> {
    let directory = std::path::absolute(directory)?;
    let script = format!(
        r#"$entry={}; $saved=[Environment]::GetEnvironmentVariable('Path','User'); if (@($saved -split ';') -notcontains $entry) {{ $next=if([string]::IsNullOrEmpty($saved)){{$entry}}else{{$saved.TrimEnd(';')+';'+$entry}}; [Environment]::SetEnvironmentVariable('Path',$next,'User'); 'updated' }}"#,
        ps_quote(&directory.to_string_lossy())
    );
    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .output()
        .context("cannot initialize the Windows user PATH")?;
    anyhow::ensure!(
        output.status.success(),
        "cannot initialize the Windows user PATH; install-self --no-shell-init leaves it untouched"
    );
    if !output.stdout.is_empty() {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn SendMessageTimeoutW(
                window: *mut std::ffi::c_void,
                message: u32,
                wparam: usize,
                lparam: isize,
                flags: u32,
                timeout: u32,
                result: *mut usize,
            ) -> isize;
        }
        let environment: Vec<u16> = "Environment".encode_utf16().chain(Some(0)).collect();
        let mut result = 0;
        // SAFETY: the broadcast contains a live NUL-terminated environment label
        // and a valid output pointer. Abort waiting for an unresponsive window.
        unsafe {
            SendMessageTimeoutW(
                0xffffusize as *mut _,
                0x1a,
                0,
                environment.as_ptr() as isize,
                2,
                1000,
                &mut result,
            )
        };
    }
    Ok(())
}

pub fn windows_path(path: &Path) -> Result<String> {
    let result = Command::new("wslpath")
        .arg("-w")
        .arg(path)
        .output()
        .context("Windows paths require WSL with wslpath")?;
    anyhow::ensure!(
        result.status.success(),
        "cannot translate workspace to a Windows path"
    );
    let path = String::from_utf8(result.stdout)?
        .trim_end_matches(['\r', '\n'])
        .to_owned();
    anyhow::ensure!(
        !path.is_empty() && !path.contains(['\r', '\n', '"']),
        "workspace has an unsupported Windows path"
    );
    Ok(path)
}

fn wsl_cwd(path: &str, selected: Option<&str>) -> Result<(String, Option<String>)> {
    let normalized = path.replace('/', "\\");
    let plain = normalized
        .strip_prefix("\\\\?\\UNC\\")
        .map(|p| format!("\\\\{p}"))
        .unwrap_or(normalized);
    for prefix in ["\\\\wsl.localhost\\", "\\\\wsl$\\"] {
        if plain.to_ascii_lowercase().starts_with(prefix) {
            let rest = &plain[prefix.len()..];
            let (distro, tail) = rest.split_once('\\').unwrap_or((rest, ""));
            anyhow::ensure!(selected.is_none_or(|s|s.eq_ignore_ascii_case(distro)),"current directory belongs to another WSL distribution; select that distribution or change directory");
            return Ok((
                format!("/{}", tail.replace('\\', "/")),
                Some(distro.to_owned()),
            ));
        }
    }
    Ok((path.to_owned(), selected.map(str::to_owned)))
}

pub fn dispatch_wsl(arguments: &[OsString]) -> Result<()> {
    let selected = std::env::var("AI_FENCE_WORKSPACE_WSL_DISTRO").ok();
    let cli = std::env::var_os("AI_FENCE_WORKSPACE_WSL_CLI")
        .unwrap_or_else(|| "~/.local/bin/ai-fence-cli".into());
    let cwd = std::env::current_dir()?;
    #[cfg(windows)]
    let cwd = mapped_windows_path(&cwd)?;
    let (cwd, distro) = wsl_cwd(&cwd.to_string_lossy(), selected.as_deref())?;
    let mut command = Command::new("wsl.exe");
    if let Some(distro) = distro {
        command.args(["--distribution", &distro]);
    }
    command.arg("--cd").arg(cwd).args(["--exec","/bin/sh","-c",r#"cli=$1; shift; case "$cli" in '~/'*) cli="$HOME/${cli#\~/}";; esac; exec "$cli" workspace "$@""#,"ai-fence-workspace"]).arg(cli).args(arguments);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("cannot start WSL workspace CLI")
    }
    #[cfg(not(unix))]
    {
        let status=command.status().context("cannot start WSL; install a Linux AI Fence CLI and configure AI_FENCE_WORKSPACE_WSL_DISTRO")?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// CMD pushd maps UNC directories to a drive letter. Resolve that mapping before
/// asking WSL to select a distribution/cwd; never fall back to another project.
#[cfg(windows)]
fn mapped_windows_path(path: &Path) -> Result<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    if path.to_string_lossy().starts_with(r"\\") {
        return Ok(path.to_path_buf());
    }
    #[link(name = "mpr")]
    unsafe extern "system" {
        fn WNetGetUniversalNameW(
            path: *const u16,
            level: u32,
            buffer: *mut std::ffi::c_void,
            size: *mut u32,
        ) -> u32;
    }
    let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut size = 0;
    // SAFETY: the input is NUL terminated; a null buffer requests the needed size.
    let status =
        unsafe { WNetGetUniversalNameW(wide.as_ptr(), 1, std::ptr::null_mut(), &mut size) };
    if status == 2250 {
        return Ok(path.to_path_buf());
    } // ERROR_NOT_CONNECTED: local/UNC path.
    anyhow::ensure!(
        status == 234 && size > 0 && size <= 65536,
        "cannot resolve Windows workspace drive mapping (error {status})"
    );
    let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: the buffer is pointer-aligned and at least the requested byte size.
    let status =
        unsafe { WNetGetUniversalNameW(wide.as_ptr(), 1, buffer.as_mut_ptr().cast(), &mut size) };
    anyhow::ensure!(
        status == 0,
        "cannot resolve Windows workspace drive mapping (error {status})"
    );
    // UNIVERSAL_NAME_INFO contains one pointer into this buffer.
    // SAFETY: the successful level-1 call initialized that pointer.
    let start = unsafe { *(buffer.as_ptr() as *const *const u16) };
    let lower = buffer.as_ptr() as usize;
    let upper = lower + buffer.len() * std::mem::size_of::<usize>();
    let address = start as usize;
    anyhow::ensure!(
        address >= lower && address < upper && address.is_multiple_of(2),
        "invalid Windows workspace mapping response"
    );
    // SAFETY: bound the UTF-16 view to the still-live allocation before reading.
    let value = unsafe { std::slice::from_raw_parts(start, (upper - address) / 2) };
    let end = value
        .iter()
        .position(|v| *v == 0)
        .context("unterminated Windows workspace mapping")?;
    Ok(PathBuf::from(OsString::from_wide(&value[..end])))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn powershell_utf16_profiles_preserve_encoding_and_custom_unicode() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profile.ps1");
        let initial = "# My profile: λ 日本語\r\n";
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend(initial.encode_utf16().flat_map(u16::to_le_bytes));
        fs::write(&profile, &bytes).unwrap();
        initialize_profile(&profile, ". 'workspace.ps1'", None).unwrap();
        let saved = fs::read(&profile).unwrap();
        assert!(saved.starts_with(&bytes));
        initialize_profile(&profile, ". 'workspace.ps1'", None).unwrap();
        assert_eq!(fs::read(&profile).unwrap(), saved);
    }
    #[test]
    fn unc_aliases_select_the_owning_distribution_without_fallback() {
        for root in [
            r"\\wsl.localhost\Example",
            r"\\wsl$\Example",
            r"\\?\UNC\wsl.localhost\Example",
        ] {
            assert_eq!(
                wsl_cwd(&format!(r"{root}\home\user\project"), None).unwrap(),
                ("/home/user/project".into(), Some("Example".into()))
            );
            assert!(wsl_cwd(root, Some("Another")).is_err());
        }
        assert_eq!(
            wsl_cwd(r"C:\project", Some("Example")).unwrap(),
            (r"C:\project".into(), Some("Example".into()))
        );
    }
    #[test]
    fn shell_profile_install_is_idempotent_and_preserves_user_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profile");
        fs::write(&profile, "# my custom profile\nexport KEEP=1\n").unwrap();
        initialize_profile(&profile, ". '/first'", None).unwrap();
        initialize_profile(&profile, ". '/second'", None).unwrap();
        let content = fs::read_to_string(&profile).unwrap();
        assert!(content.starts_with("# my custom profile\nexport KEEP=1\n"));
        assert_eq!(content.matches("# >>> AI Fence").count(), 1);
        assert!(!content.contains("/first"));
        assert!(content.contains("/second"));
        initialize_profile(&profile, ". '/dev'", Some("dev")).unwrap();
        assert!(fs::read_to_string(&profile).unwrap().contains("/second"));
    }
}
