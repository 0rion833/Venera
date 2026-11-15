//! Adds the executable to the user's `PATH` on the first terminal launch.
//!
//! Windows writes the per-user `Path` value (`HKCU\Environment`) and broadcasts
//! `WM_SETTINGCHANGE` — the same effect as `setx`, but without its 1024-character
//! truncation. Unix links the binary into `~/.local/bin` and appends a marked
//! block to the shell profile when that directory is not on `PATH` yet.
//!
//! The setup is best effort and never blocks a launch. It only runs when stdout
//! is a terminal, so piped CLI calls and the test suite never edit the user's
//! environment.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

/// What happened to the launcher entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The launcher was added; a new terminal is needed to see it.
    Added,
    /// The launcher was already reachable.
    Present,
    /// Nothing was changed: piped output, a missing tool, or an error.
    Skipped,
}

/// Ensures the executable is reachable by name; never fails the launch.
pub fn install() -> Outcome {
    if std::env::var_os("VENERA_NO_PATH_SETUP").is_some() || !std::io::stdout().is_terminal() {
        return Outcome::Skipped;
    }

    register()
}

#[cfg(windows)]
fn register() -> Outcome {
    windows::register()
}

#[cfg(unix)]
fn register() -> Outcome {
    unix::register()
}

#[cfg(not(any(windows, unix)))]
fn register() -> Outcome {
    Outcome::Skipped
}

/// Directory of the running binary.
#[cfg(windows)]
fn exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.parent().map(Path::to_path_buf)
}

/// Whether `dir` is one of the entries of `path_value`.
///
/// Entries are compared after expanding Windows variables, folding `\` to `/`,
/// dropping a trailing separator, and optionally ignoring case.
fn contains_dir(path_value: &str, dir: &str, separator: char, case_insensitive: bool) -> bool {
    let wanted = normalize_entry(dir, case_insensitive);
    !wanted.is_empty()
        && path_value
            .split(separator)
            .any(|entry| normalize_entry(entry, case_insensitive) == wanted)
}

fn normalize_entry(entry: &str, case_insensitive: bool) -> String {
    let entry = entry.trim().trim_matches('"');
    let entry = expand(entry);
    let mut entry = entry.replace('\\', "/");
    while entry.len() > 3 && entry.ends_with('/') {
        entry.pop();
    }

    if case_insensitive {
        entry.to_lowercase()
    } else {
        entry
    }
}

/// Expands `%NAME%` references, the way Windows does for `PATH`.
#[cfg(windows)]
fn expand(value: &str) -> String {
    let mut expanded = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(start) = rest.find('%') {
        expanded.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let Some(end) = tail.find('%') else {
            expanded.push('%');
            expanded.push_str(tail);
            return expanded;
        };

        let name = &tail[..end];
        match std::env::var_os(name) {
            Some(replacement) => expanded.push_str(&replacement.to_string_lossy()),
            None => {
                expanded.push('%');
                expanded.push_str(name);
                expanded.push('%');
            }
        }
        rest = &tail[end + 1..];
    }

    expanded.push_str(rest);
    expanded
}

#[cfg(not(windows))]
fn expand(value: &str) -> String {
    value.to_owned()
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::process::Command;

    pub(super) fn register() -> Outcome {
        let Some(dir) = exe_dir() else {
            return Outcome::Skipped;
        };
        let dir = dir.to_string_lossy().into_owned();

        let current = std::env::var_os("PATH")
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        if contains_dir(&current, &dir, ';', true) {
            return Outcome::Present;
        }

        let Some(user) = read_user_path() else {
            return Outcome::Skipped;
        };
        if contains_dir(&user, &dir, ';', true) {
            return Outcome::Present;
        }

        let updated = append_path(&user, &dir);
        if !write_user_path(&updated) {
            return Outcome::Skipped;
        }
        broadcast();
        Outcome::Added
    }

    fn read_user_path() -> Option<String> {
        const SCRIPT: &str = "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; $key=[Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment'); if($null -ne $key){$key.GetValue('Path','',[Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)}";
        let output = powershell(SCRIPT, None)?;
        if !output.status.success() {
            return None;
        }

        let text = String::from_utf8_lossy(&output.stdout);
        Some(
            text.trim_start_matches('\u{feff}')
                .trim_end_matches(['\r', '\n'])
                .to_owned(),
        )
    }

    fn write_user_path(value: &str) -> bool {
        const SCRIPT: &str = "$ErrorActionPreference='Stop'; $key=[Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment',$true); if($null -eq $key){$key=[Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')}; $kind=if(($key.GetValueNames() -contains 'Path')){$key.GetValueKind('Path')}else{[Microsoft.Win32.RegistryValueKind]::ExpandString}; $key.SetValue('Path',$env:VENERA_NEW_PATH,$kind); $key.Close()";
        let Some(output) = powershell(SCRIPT, Some(("VENERA_NEW_PATH", value))) else {
            return false;
        };
        output.status.success()
    }

    /// Lets Explorer pick the new `Path` up without a logout.
    fn broadcast() {
        const SCRIPT: &str = r#"$signature='[System.Runtime.InteropServices.DllImport("user32.dll", CharSet=System.Runtime.InteropServices.CharSet.Unicode, SetLastError=true)] public static extern System.IntPtr SendMessageTimeout(System.IntPtr hWnd, uint Msg, System.UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out System.UIntPtr lpdwResult);'; Add-Type -Namespace VeneraWin -Name Native -MemberDefinition $signature; $result=[UIntPtr]::Zero; [VeneraWin.Native]::SendMessageTimeout([IntPtr]0xffff,0x001A,[UIntPtr]::Zero,'Environment',2,5000,[ref]$result) | Out-Null"#;
        let _ = powershell(SCRIPT, None);
    }

    fn powershell(script: &str, env: Option<(&str, &str)>) -> Option<std::process::Output> {
        let encoded = encode_powershell(script);

        for binary in ["powershell", "pwsh"] {
            let mut command = Command::new(binary);
            command.args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded]);
            if let Some((name, value)) = env {
                command.env(name, value);
            }
            if let Ok(output) = command.output() {
                return Some(output);
            }
        }

        None
    }

    fn encode_powershell(script: &str) -> String {
        use base64::Engine as _;

        let mut bytes = Vec::with_capacity(script.len() * 2);
        for unit in script.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }

        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    pub(super) fn append_path(value: &str, dir: &str) -> String {
        let value = value.trim_end();
        if value.is_empty() {
            return dir.to_owned();
        }
        if value.ends_with(';') {
            format!("{value}{dir}")
        } else {
            format!("{value};{dir}")
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::symlink;

    const MARK_START: &str = "# >>> venera >>>";
    const MARK_END: &str = "# <<< venera <<<";

    pub(super) fn register() -> Outcome {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return Outcome::Skipped;
        };
        let bin = home.join(".local").join("bin");
        if fs::create_dir_all(&bin).is_err() {
            return Outcome::Skipped;
        }

        let target = std::env::var_os("APPIMAGE")
            .map(PathBuf::from)
            .or_else(|| std::env::current_exe().ok());
        let Some(target) = target else {
            return Outcome::Skipped;
        };

        let link = bin.join("venera");
        if target != link && !ensure_link(&link, &target) {
            return Outcome::Skipped;
        }

        let path = std::env::var_os("PATH")
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        if contains_dir(&path, &bin.to_string_lossy(), ':', false) {
            return Outcome::Present;
        }

        let (profile, fish) = profile(&home);
        let line = if fish {
            "fish_add_path \"$HOME/.local/bin\""
        } else {
            "export PATH=\"$HOME/.local/bin:$PATH\""
        };

        match add_block(&profile, line) {
            Some(true) => Outcome::Added,
            Some(false) => Outcome::Present,
            None => Outcome::Skipped,
        }
    }

    /// Points `~/.local/bin/venera` at the running binary without touching files
    /// that do not belong to Venera.
    fn ensure_link(link: &Path, target: &Path) -> bool {
        match fs::symlink_metadata(link) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let same = fs::read_link(link)
                    .map(|current| current == target)
                    .unwrap_or(false);
                if same {
                    return true;
                }
                if fs::remove_file(link).is_err() {
                    return false;
                }
                symlink(target, link).is_ok()
            }
            Ok(_) => false,
            Err(_) => symlink(target, link).is_ok(),
        }
    }

    /// The profile file of the login shell, and whether it speaks fish.
    fn profile(home: &Path) -> (PathBuf, bool) {
        let shell = std::env::var_os("SHELL")
            .and_then(|value| {
                PathBuf::from(value)
                    .file_name()
                    .map(|name| name.to_string_lossy().to_lowercase())
            })
            .unwrap_or_default();

        match shell.as_str() {
            "zsh" => (home.join(".zshrc"), false),
            "bash" => (home.join(".bashrc"), false),
            "fish" => (home.join(".config").join("fish").join("config.fish"), true),
            _ => (home.join(".profile"), false),
        }
    }

    /// Appends the marked block once.
    ///
    /// `Some(false)` means the block is already there, `None` that the profile
    /// could not be written.
    fn add_block(path: &Path, line: &str) -> Option<bool> {
        let existing = fs::read_to_string(path).unwrap_or_default();
        if existing.contains(MARK_START) {
            return Some(false);
        }
        if let Some(parent) = path.parent()
            && fs::create_dir_all(parent).is_err()
        {
            return None;
        }

        let mut block = String::new();
        if !existing.is_empty() && !existing.ends_with('\n') {
            block.push('\n');
        }
        block.push_str(MARK_START);
        block.push('\n');
        block.push_str(line);
        block.push('\n');
        block.push_str(MARK_END);
        block.push('\n');

        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(block.as_bytes()))
            .ok()
            .map(|()| true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_entry_is_found_with_a_trailing_separator() {
        assert!(contains_dir(
            "/usr/bin:/opt/tools/",
            "/opt/tools",
            ':',
            false
        ));
        assert!(!contains_dir(
            "/usr/bin:/opt/tools",
            "/opt/tool",
            ':',
            false
        ));
    }

    #[test]
    fn windows_entries_compare_case_insensitively_when_asked() {
        assert!(contains_dir(
            r"C:\Tools\Venera\;D:\Other",
            r"c:\tools\venera",
            ';',
            true
        ));
        assert!(!contains_dir(
            r"C:\Tools\Venera",
            r"C:\Tools\venera",
            ';',
            false
        ));
    }

    #[cfg(windows)]
    #[test]
    fn appending_keeps_existing_entries() {
        assert_eq!(
            windows::append_path(r"C:\one;D:\two", r"C:\venera"),
            r"C:\one;D:\two;C:\venera"
        );
        assert_eq!(
            windows::append_path(r"C:\one;", r"C:\venera"),
            r"C:\one;C:\venera"
        );
        assert_eq!(windows::append_path("", r"C:\venera"), r"C:\venera");
    }
}
