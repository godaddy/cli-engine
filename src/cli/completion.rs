use std::path::{Path, PathBuf};

use clap::Command;
use clap_complete::{Shell as ClapShell, generate};

use crate::CliRunOutput;
use crate::error::CliCoreError;

/// The shells supported by the completion built-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// `PowerShell` ends with the enum name `Shell`; the name is intentional.
#[allow(clippy::enum_variant_names)]
pub(crate) enum Shell {
    /// Bourne-Again Shell.
    Bash,
    /// Z Shell.
    Zsh,
    /// Friendly Interactive Shell.
    Fish,
    /// PowerShell.
    PowerShell,
    /// Elvish shell.
    Elvish,
}

/// Parses a shell name (case-insensitive) into a [`Shell`] variant.
///
/// Returns an error for unrecognized names rather than panicking.
pub(crate) fn parse_shell(s: &str) -> crate::Result<Shell> {
    match s.to_ascii_lowercase().as_str() {
        "bash" => Ok(Shell::Bash),
        "zsh" => Ok(Shell::Zsh),
        "fish" => Ok(Shell::Fish),
        // `pwsh` is the PowerShell Core executable name seen in $SHELL/argv.
        "powershell" | "pwsh" => Ok(Shell::PowerShell),
        "elvish" => Ok(Shell::Elvish),
        _ => Err(CliCoreError::Message(format!(
            "unsupported shell: {s}; supported: bash, zsh, fish, powershell, elvish"
        ))),
    }
}

/// Basename split on both Unix and Windows separators, with a trailing `.exe` stripped
/// and a version suffix like `-5.9` removed (e.g. `"zsh-5.9"` → `"zsh"`).
fn shell_basename(path: &str) -> &str {
    let basename = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let name = basename
        .strip_suffix(".exe")
        .or_else(|| basename.strip_suffix(".EXE"))
        .unwrap_or(basename);
    // Strip version suffix like "-5.9" so versioned shell paths (e.g. "/usr/bin/zsh-5.9") work.
    // Use rfind so a name like "fish-shell-3.7" strips only the trailing "-3.7", yielding
    // "fish-shell" rather than "fish".
    if let Some(idx) = name.rfind('-')
        && name[idx + 1..].starts_with(|c: char| c.is_ascii_digit())
    {
        return &name[..idx];
    }
    name
}

/// Detects the current shell by inspecting the `$SHELL` environment variable.
///
/// On Windows, defaults to [`Shell::PowerShell`] when `$SHELL` is unset.
/// Returns an error if the shell is unset on non-Windows or if the detected
/// name is not a recognized shell.
pub(crate) fn detect_shell() -> crate::Result<Shell> {
    let shell_var = std::env::var("SHELL").ok().filter(|s| !s.is_empty());

    match shell_var {
        Some(path) => {
            let basename = shell_basename(&path);
            parse_shell(basename).map_err(|_| {
                CliCoreError::Message(format!(
                    "could not detect shell: $SHELL is set to {path:?} but that is not a recognized shell; supported: bash, zsh, fish, powershell, elvish"
                ))
            })
        }
        None => {
            if cfg!(windows) {
                Ok(Shell::PowerShell)
            } else {
                Err(CliCoreError::Message(
                    "could not detect shell: $SHELL is not set".to_owned(),
                ))
            }
        }
    }
}

/// Maps a [`Shell`] to the corresponding [`ClapShell`] variant.
fn to_clap_shell(shell: Shell) -> ClapShell {
    match shell {
        Shell::Bash => ClapShell::Bash,
        Shell::Zsh => ClapShell::Zsh,
        Shell::Fish => ClapShell::Fish,
        Shell::PowerShell => ClapShell::PowerShell,
        Shell::Elvish => ClapShell::Elvish,
    }
}

/// Generates a shell completion script for the given root [`Command`].
///
/// Clones the command internally so the caller's instance is not mutated.
///
/// # Errors
///
/// Returns an error if `clap_complete` produces non-UTF-8 output (this is a
/// bug in the upstream crate, but surfacing it is safer than silently writing
/// a corrupted script).
pub(crate) fn generate_script(
    root: &Command,
    bin_name: &str,
    shell: Shell,
) -> crate::Result<String> {
    let clap_shell = to_clap_shell(shell);
    let mut buf: Vec<u8> = Vec::new();
    generate(clap_shell, &mut root.clone(), bin_name, &mut buf);
    String::from_utf8(buf)
        .map_err(|e| CliCoreError::message(format!("completion script is not valid UTF-8: {e}")))
}

/// Resolves `$XDG_DATA_HOME` with fallback to `$HOME/.local/share`.
///
/// Only absolute paths are accepted.
fn xdg_data_dir() -> Option<PathBuf> {
    let xdg = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    xdg.or_else(|| crate::fs::home_dir().map(|h| h.join(".local").join("share")))
}

/// Inserts or replaces a managed block delimited by `bin_name`-specific markers
/// inside `content`.
///
/// - If a complete begin+end pair is found, the span (inclusive) is replaced.
/// - If only the begin marker is found (end was deleted), the orphaned begin
///   line is removed before the new complete block is appended, preventing
///   duplicate markers on re-runs.
/// - If neither marker is present, the block is appended.
fn apply_managed_block(content: &str, bin_name: &str, body: &str) -> String {
    let begin = format!("# >>> {bin_name} completion (managed) >>>");
    let end = format!("# <<< {bin_name} completion (managed) <<<");
    let new_block = format!("{begin}\n{body}\n{end}");

    // Only treat the end marker that follows the begin marker as the span end,
    // so a stray earlier end marker cannot delete unrelated content.
    if let Some(start_idx) = content.find(&begin)
        && let Some(rel_end) = content[start_idx..].find(&end)
    {
        let end_idx = start_idx + rel_end + end.len();
        format!(
            "{}{new_block}{}",
            &content[..start_idx],
            &content[end_idx..]
        )
    } else if let Some(start_idx) = content.find(&begin) {
        // Begin marker present but end marker missing; remove the orphaned begin
        // line so repeated installs don't accumulate stray markers.  Handle
        // both LF and CRLF line endings after the marker.
        let after_begin = start_idx + begin.len();
        let after_line = if content[after_begin..].starts_with("\r\n") {
            after_begin + 2
        } else if content[after_begin..].starts_with('\n') {
            after_begin + 1
        } else {
            after_begin
        };
        format!(
            "{}{}\n\n{new_block}\n",
            &content[..start_idx],
            &content[after_line..]
        )
    } else {
        format!("{content}\n\n{new_block}\n")
    }
}

/// Reads existing rc file content, resolving symlinks.
///
/// Returns `(canonical_path, content)`. If the file does not exist, returns
/// the original path with empty content. If the file exists but cannot be
/// read (e.g. permission denied), returns an error so the caller does not
/// accidentally overwrite the file with only the managed block.
fn read_rc(rc_path: &Path) -> crate::Result<(PathBuf, String)> {
    match std::fs::canonicalize(rc_path) {
        Ok(canonical) => {
            // File exists; a read failure here (e.g. permission denied) must
            // not be silently treated as "empty" — that would destroy the
            // user's config when write_string_atomic overwrites it.
            let content = std::fs::read_to_string(&canonical).map_err(|e| {
                CliCoreError::message(format!("cannot read {}: {e}", canonical.display()))
            })?;
            Ok((canonical, content))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // File genuinely does not exist; start with empty content.
            Ok((rc_path.to_owned(), String::new()))
        }
        Err(e) => Err(CliCoreError::message(format!(
            "cannot resolve {}: {e}",
            rc_path.display()
        ))),
    }
}

/// Per-shell rc-file specification, computed on the async thread (env var reads
/// only) and consumed inside `spawn_blocking` where the actual file I/O occurs.
struct RcSpec {
    /// Destination rc file path (not yet canonicalized).
    path: PathBuf,
    /// Body to wrap inside the managed-block markers.
    body: String,
    /// Normalize the full rc file content to CRLF after applying the block
    /// (required by PowerShell).
    crlf: bool,
}

/// Installs shell completions for `bin_name` into the appropriate per-shell
/// location.
///
/// For shells that source rc files (bash, zsh, elvish, powershell) an idempotent
/// managed block is inserted or updated in the rc file. Fish auto-loads
/// completions from its completions directory so no rc edit is needed.
///
/// Path resolution (env var reads) happens on the calling async thread; all
/// file I/O is offloaded to a single [`tokio::task::spawn_blocking`] closure so
/// the executor thread is never blocked.
///
/// # Errors
///
/// Returns an error if required home/config directories cannot be resolved, if
/// an existing rc file cannot be read, or if any file write fails.
pub(crate) async fn install(
    root: &Command,
    bin_name: &str,
    shell: Shell,
) -> crate::Result<CliRunOutput> {
    let script = generate_script(root, bin_name, shell)?;
    let bin_name = bin_name.to_owned();

    // Compute per-shell paths (env var reads only — no blocking I/O here).
    let (script_path, rc_spec): (PathBuf, Option<RcSpec>) = match shell {
        Shell::Bash => {
            let data = xdg_data_dir().ok_or_else(|| {
                CliCoreError::message("could not resolve XDG_DATA_HOME or HOME for bash completion")
            })?;
            let script_path = data.join("bash-completion/completions").join(&bin_name);
            let rc_path = crate::fs::home_dir()
                .ok_or_else(|| CliCoreError::message("could not resolve HOME for .bashrc"))?
                .join(".bashrc");
            let body = format!("source \"{}\"", script_path.display());
            (
                script_path,
                Some(RcSpec {
                    path: rc_path,
                    body,
                    crlf: false,
                }),
            )
        }

        Shell::Zsh => {
            let home = crate::fs::home_dir().ok_or_else(|| {
                CliCoreError::message("could not resolve HOME for zsh completion")
            })?;
            let script_path = home.join(".zfunc").join(format!("_{bin_name}"));
            // When $ZDOTDIR is set, zsh reads its dotfiles from that directory
            // instead of $HOME.  Write to $ZDOTDIR/.zshrc so the sourced block
            // actually takes effect.
            let zshrc_dir = std::env::var("ZDOTDIR")
                .ok()
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.clone());
            let rc_path = zshrc_dir.join(".zshrc");
            let body = format!(
                "fpath=(\"{home}/.zfunc\" $fpath)\nautoload -Uz compinit && compinit",
                home = home.display()
            );
            (
                script_path,
                Some(RcSpec {
                    path: rc_path,
                    body,
                    crlf: false,
                }),
            )
        }

        Shell::Fish => {
            let config = crate::fs::config_base_dir().ok_or_else(|| {
                CliCoreError::message(
                    "could not resolve XDG_CONFIG_HOME or HOME for fish completion",
                )
            })?;
            let script_path = config
                .join("fish/completions")
                .join(format!("{bin_name}.fish"));
            // Fish auto-loads from this directory; no rc edit required.
            (script_path, None)
        }

        Shell::Elvish => {
            let config = crate::fs::config_base_dir().ok_or_else(|| {
                CliCoreError::message(
                    "could not resolve XDG_CONFIG_HOME or HOME for elvish completion",
                )
            })?;
            let script_path = config
                .join("elvish/lib")
                .join(format!("{bin_name}-completion.elv"));
            let rc_path = config.join("elvish/rc.elv");
            let body = format!("use {bin_name}-completion");
            (
                script_path,
                Some(RcSpec {
                    path: rc_path,
                    body,
                    crlf: false,
                }),
            )
        }

        Shell::PowerShell => {
            let home = crate::fs::home_dir().ok_or_else(|| {
                CliCoreError::message("could not resolve HOME for powershell completion")
            })?;
            let profile_path = home
                .join("Documents")
                .join("PowerShell")
                .join("Microsoft.PowerShell_profile.ps1");
            let profile_dir = profile_path
                .parent()
                .ok_or_else(|| {
                    CliCoreError::message("could not determine PowerShell profile directory")
                })?
                .to_owned();
            let script_path = profile_dir.join(format!("{bin_name}-completion.ps1"));
            let body = format!(". \"{}\"", script_path.display());
            // Normalize to LF first so an existing CRLF profile is not turned into
            // `\r\r\n`, then emit CRLF as PowerShell expects.
            (
                script_path,
                Some(RcSpec {
                    path: profile_path,
                    body,
                    crlf: true,
                }),
            )
        }
    };

    // All file I/O in one spawn_blocking: write script, read rc, apply block, write rc.
    let script_path_clone = script_path.clone();
    let bin_name_for_block = bin_name.clone();
    let written = tokio::task::spawn_blocking(move || -> crate::Result<Vec<String>> {
        crate::fs::write_string_atomic(&script_path_clone, &script).map_err(|e| {
            CliCoreError::message(format!("failed to write completion script: {e}"))
        })?;
        let mut written = vec![script_path_clone.display().to_string()];

        if let Some(rc) = rc_spec {
            let (canonical_rc, existing) = read_rc(&rc.path)?;
            let new_content = apply_managed_block(&existing, &bin_name_for_block, &rc.body);
            let final_content = if rc.crlf {
                new_content.replace("\r\n", "\n").replace('\n', "\r\n")
            } else {
                new_content
            };
            crate::fs::write_string_atomic(&canonical_rc, &final_content)
                .map_err(|e| CliCoreError::message(format!("failed to write rc file: {e}")))?;
            written.push(canonical_rc.display().to_string());
        }

        Ok(written)
    })
    .await
    .map_err(|e| CliCoreError::message(format!("spawn_blocking join error: {e}")))??;

    Ok(CliRunOutput {
        exit_code: 0,
        rendered: format!(
            "Installed {bin_name} completion.\nFiles written:\n{}",
            written.join("\n")
        ),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;

    #[test]
    fn parse_shell_from_full_path() {
        assert_eq!(parse_shell("zsh").unwrap(), Shell::Zsh);
        assert_eq!(parse_shell("bash").unwrap(), Shell::Bash);
    }

    #[test]
    fn parse_shell_case_insensitive() {
        assert_eq!(parse_shell("Bash").unwrap(), Shell::Bash);
        assert_eq!(parse_shell("ZSH").unwrap(), Shell::Zsh);
        assert!(parse_shell("notashell").is_err());
    }

    #[test]
    fn parse_shell_accepts_pwsh_alias() {
        assert_eq!(parse_shell("pwsh").unwrap(), Shell::PowerShell);
        assert_eq!(parse_shell("PWSH").unwrap(), Shell::PowerShell);
        assert_eq!(parse_shell("powershell").unwrap(), Shell::PowerShell);
    }

    #[test]
    fn shell_basename_handles_windows_paths_and_exe() {
        assert_eq!(shell_basename("/usr/bin/bash"), "bash");
        assert_eq!(
            shell_basename("C:\\Program Files\\PowerShell\\pwsh.exe"),
            "pwsh"
        );
        assert_eq!(shell_basename("pwsh.EXE"), "pwsh");
        assert_eq!(shell_basename("zsh"), "zsh");
    }

    #[test]
    fn shell_basename_strips_version_suffix() {
        assert_eq!(shell_basename("/usr/bin/zsh-5.9"), "zsh");
        assert_eq!(shell_basename("bash-5.1"), "bash");
        // Non-version hyphens (no digit after) are left intact.
        assert_eq!(shell_basename("my-shell"), "my-shell");
        // A hyphen in the base name before the version suffix is preserved
        // because rfind picks the last hyphen, not the first.
        assert_eq!(shell_basename("fish-shell-3.7"), "fish-shell");
    }

    #[test]
    fn generate_script_returns_nonempty() {
        let cmd = Command::new("demo").subcommand(Command::new("list"));
        let script = generate_script(&cmd, "demo", Shell::Bash).unwrap();
        assert!(!script.is_empty(), "script should be non-empty");
        assert!(script.contains("demo"), "script should mention bin name");
    }

    #[test]
    fn managed_block_appended_when_absent() {
        let result = apply_managed_block("existing content", "mybin", "source /path/to/script");
        assert!(result.contains("# >>> mybin completion (managed) >>>"));
        assert!(result.contains("# <<< mybin completion (managed) <<<"));
        assert!(result.contains("source /path/to/script"));
        assert!(result.contains("existing content"));
    }

    #[test]
    fn managed_block_replaced_when_present() {
        let initial = "prefix\n# >>> mybin completion (managed) >>>\nold body\n# <<< mybin completion (managed) <<<\nsuffix";
        let result = apply_managed_block(initial, "mybin", "new body");
        assert_eq!(
            result
                .matches("# >>> mybin completion (managed) >>>")
                .count(),
            1
        );
        assert!(!result.contains("old body"), "old body should be replaced");
        assert!(result.contains("new body"), "new body should appear");
        assert!(result.contains("prefix"), "prefix should be preserved");
        assert!(result.contains("suffix"), "suffix should be preserved");
    }

    #[test]
    fn managed_block_ignores_stray_end_marker_before_begin() {
        let initial = "# <<< mybin completion (managed) <<<\nimportant user content\n# >>> mybin completion (managed) >>>\nold body\n# <<< mybin completion (managed) <<<";
        let result = apply_managed_block(initial, "mybin", "new body");
        assert!(
            result.contains("important user content"),
            "content between a stray end marker and the real begin marker must be preserved"
        );
        assert!(result.contains("new body"), "new body should appear");
        assert!(!result.contains("old body"), "old body should be replaced");
    }

    #[test]
    fn managed_block_replaces_orphaned_begin_marker() {
        // User deleted the end marker; re-running install must not accumulate
        // a second begin marker.  Only the begin line is removed (not any
        // content after it) because without the end marker the block extent
        // is unknown.
        let initial = "prefix\n# >>> mybin completion (managed) >>>\nsuffix";
        let result = apply_managed_block(initial, "mybin", "new body");
        assert_eq!(
            result
                .matches("# >>> mybin completion (managed) >>>")
                .count(),
            1,
            "exactly one begin marker after replacing orphaned block"
        );
        assert!(result.contains("# <<< mybin completion (managed) <<<"));
        assert!(result.contains("new body"), "new body should appear");
        assert!(result.contains("prefix"), "prefix should be preserved");
        assert!(result.contains("suffix"), "suffix should be preserved");
    }

    #[allow(unsafe_code, clippy::await_holding_lock)]
    #[tokio::test]
    async fn install_bash_writes_script_and_rc() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let config_dir = tmp.path().join("config");

        let _lock = crate::config::test_env::lock();
        let prev_home = std::env::var("HOME").ok();
        let prev_data = std::env::var("XDG_DATA_HOME").ok();
        let prev_config = std::env::var("XDG_CONFIG_HOME").ok();
        // SAFETY: caller holds XDG_TEST_MUTEX, serializing all mutation.
        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
            std::env::set_var("XDG_DATA_HOME", data_dir.to_str().unwrap());
            std::env::set_var("XDG_CONFIG_HOME", config_dir.to_str().unwrap());
        }

        let cmd = Command::new("testbin").subcommand(Command::new("list"));
        let result = install(&cmd, "testbin", Shell::Bash).await.unwrap();
        assert_eq!(result.exit_code, 0);

        let script = data_dir.join("bash-completion/completions/testbin");
        assert!(
            script.exists(),
            "script file should exist at {}",
            script.display()
        );

        let bashrc = home.join(".bashrc");
        let bashrc_content = std::fs::read_to_string(&bashrc).unwrap();
        assert!(bashrc_content.contains("# >>> testbin completion (managed) >>>"));
        assert!(bashrc_content.contains("# <<< testbin completion (managed) <<<"));

        install(&cmd, "testbin", Shell::Bash).await.unwrap();
        let content2 = std::fs::read_to_string(&bashrc).unwrap();
        assert_eq!(
            content2
                .matches("# >>> testbin completion (managed) >>>")
                .count(),
            1
        );

        // SAFETY: restoring state while still holding the lock.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_data {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
            match prev_config {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    #[allow(unsafe_code, clippy::await_holding_lock)]
    #[tokio::test]
    async fn install_zsh_writes_script_and_rc() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();

        let _lock = crate::config::test_env::lock();
        let prev_home = std::env::var("HOME").ok();
        let prev_data = std::env::var("XDG_DATA_HOME").ok();
        let prev_config = std::env::var("XDG_CONFIG_HOME").ok();
        // SAFETY: caller holds XDG_TEST_MUTEX, serializing all mutation.
        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
            std::env::set_var("XDG_DATA_HOME", tmp.path().join("data").to_str().unwrap());
            std::env::set_var(
                "XDG_CONFIG_HOME",
                tmp.path().join("config").to_str().unwrap(),
            );
        }

        let cmd = Command::new("testbin");
        let result = install(&cmd, "testbin", Shell::Zsh).await.unwrap();
        assert_eq!(result.exit_code, 0);

        let script = home.join(".zfunc/_testbin");
        assert!(
            script.exists(),
            "zsh script should exist at {}",
            script.display()
        );

        let zshrc = home.join(".zshrc");
        let content = std::fs::read_to_string(&zshrc).unwrap();
        assert!(content.contains("# >>> testbin completion (managed) >>>"));
        assert!(content.contains("fpath="));
        assert!(content.contains("autoload -Uz compinit"));

        // SAFETY: restoring state while still holding the lock.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_data {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
            match prev_config {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    #[allow(unsafe_code, clippy::await_holding_lock)]
    #[tokio::test]
    async fn install_fish_writes_no_rc() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join("config");

        let _lock = crate::config::test_env::lock();
        let prev_home = std::env::var("HOME").ok();
        let prev_config = std::env::var("XDG_CONFIG_HOME").ok();
        let prev_data = std::env::var("XDG_DATA_HOME").ok();
        // SAFETY: caller holds XDG_TEST_MUTEX, serializing all mutation.
        unsafe {
            std::env::set_var("HOME", tmp.path().to_str().unwrap());
            std::env::set_var("XDG_CONFIG_HOME", config_dir.to_str().unwrap());
            std::env::set_var("XDG_DATA_HOME", tmp.path().join("data").to_str().unwrap());
        }

        let cmd = Command::new("testbin");
        let result = install(&cmd, "testbin", Shell::Fish).await.unwrap();
        assert_eq!(result.exit_code, 0);

        let script = config_dir.join("fish/completions/testbin.fish");
        assert!(
            script.exists(),
            "fish script should exist at {}",
            script.display()
        );

        let rc_candidates = ["config.fish", "init.fish"];
        for rc in rc_candidates {
            assert!(
                !config_dir.join("fish").join(rc).exists(),
                "{rc} should not exist"
            );
        }

        // SAFETY: restoring state while still holding the lock.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_config {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match prev_data {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
    }

    #[allow(unsafe_code, clippy::await_holding_lock)]
    #[tokio::test]
    async fn install_zsh_respects_zdotdir() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let zdotdir = tmp.path().join("zdotdir");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&zdotdir).unwrap();

        let _lock = crate::config::test_env::lock();
        let prev_home = std::env::var("HOME").ok();
        let prev_zdotdir = std::env::var("ZDOTDIR").ok();
        let prev_data = std::env::var("XDG_DATA_HOME").ok();
        let prev_config = std::env::var("XDG_CONFIG_HOME").ok();
        // SAFETY: caller holds XDG_TEST_MUTEX, serializing all mutation.
        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
            std::env::set_var("ZDOTDIR", zdotdir.to_str().unwrap());
            std::env::set_var("XDG_DATA_HOME", tmp.path().join("data").to_str().unwrap());
            std::env::set_var(
                "XDG_CONFIG_HOME",
                tmp.path().join("config").to_str().unwrap(),
            );
        }

        let cmd = Command::new("testbin");
        let result = install(&cmd, "testbin", Shell::Zsh).await.unwrap();
        assert_eq!(result.exit_code, 0);

        // rc should be in ZDOTDIR, not HOME
        let zshrc_in_zdotdir = zdotdir.join(".zshrc");
        assert!(
            zshrc_in_zdotdir.exists(),
            "zshrc should be written to $ZDOTDIR, not $HOME"
        );
        let zshrc_in_home = home.join(".zshrc");
        assert!(
            !zshrc_in_home.exists(),
            "zshrc must NOT be written to $HOME when $ZDOTDIR is set"
        );

        let content = std::fs::read_to_string(&zshrc_in_zdotdir).unwrap();
        assert!(content.contains("# >>> testbin completion (managed) >>>"));

        // SAFETY: restoring state while still holding the lock.
        #[allow(clippy::items_after_statements)]
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_zdotdir {
                Some(v) => std::env::set_var("ZDOTDIR", v),
                None => std::env::remove_var("ZDOTDIR"),
            }
            match prev_data {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
            match prev_config {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    #[allow(unsafe_code, clippy::await_holding_lock)]
    #[tokio::test]
    async fn install_elvish_writes_script_and_rc() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join("config");

        let _lock = crate::config::test_env::lock();
        let prev_home = std::env::var("HOME").ok();
        let prev_config = std::env::var("XDG_CONFIG_HOME").ok();
        let prev_data = std::env::var("XDG_DATA_HOME").ok();
        // SAFETY: caller holds XDG_TEST_MUTEX, serializing all mutation.
        unsafe {
            std::env::set_var("HOME", tmp.path().to_str().unwrap());
            std::env::set_var("XDG_CONFIG_HOME", config_dir.to_str().unwrap());
            std::env::set_var("XDG_DATA_HOME", tmp.path().join("data").to_str().unwrap());
        }

        let cmd = Command::new("testbin");
        let result = install(&cmd, "testbin", Shell::Elvish).await.unwrap();
        assert_eq!(result.exit_code, 0);

        let script = config_dir.join("elvish/lib/testbin-completion.elv");
        assert!(
            script.exists(),
            "elvish script should exist at {}",
            script.display()
        );

        let rc = config_dir.join("elvish/rc.elv");
        let content = std::fs::read_to_string(&rc).unwrap();
        assert!(content.contains("# >>> testbin completion (managed) >>>"));
        assert!(
            content.contains("use testbin-completion"),
            "rc must contain the use line; got:\n{content}"
        );

        // SAFETY: restoring state while still holding the lock.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_config {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match prev_data {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
    }

    #[allow(unsafe_code, clippy::await_holding_lock)]
    #[tokio::test]
    async fn install_powershell_writes_script_and_profile_with_crlf() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();

        let _lock = crate::config::test_env::lock();
        let prev_home = std::env::var("HOME").ok();
        let prev_data = std::env::var("XDG_DATA_HOME").ok();
        let prev_config = std::env::var("XDG_CONFIG_HOME").ok();
        // SAFETY: caller holds XDG_TEST_MUTEX, serializing all mutation.
        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
            std::env::set_var("XDG_DATA_HOME", tmp.path().join("data").to_str().unwrap());
            std::env::set_var(
                "XDG_CONFIG_HOME",
                tmp.path().join("config").to_str().unwrap(),
            );
        }

        let cmd = Command::new("testbin");
        let result = install(&cmd, "testbin", Shell::PowerShell).await.unwrap();
        assert_eq!(result.exit_code, 0);

        let script = home.join("Documents/PowerShell/testbin-completion.ps1");
        assert!(
            script.exists(),
            "powershell script should exist at {}",
            script.display()
        );

        let profile = home.join("Documents/PowerShell/Microsoft.PowerShell_profile.ps1");
        let content = std::fs::read_to_string(&profile).unwrap();

        // Profile must use CRLF line endings.
        assert!(
            content.contains("\r\n"),
            "profile must use CRLF line endings; got:\n{content:?}"
        );
        assert!(
            !content.contains("\r\r\n"),
            "profile must not contain double-CR (CRLF normalization bug)"
        );
        assert!(content.contains("# >>> testbin completion (managed) >>>"));
        assert!(
            content.contains(". \""),
            "profile must contain dot-source line"
        );

        // Second install must be idempotent.
        install(&cmd, "testbin", Shell::PowerShell).await.unwrap();
        let content2 = std::fs::read_to_string(&profile).unwrap();
        assert_eq!(
            content2
                .matches("# >>> testbin completion (managed) >>>")
                .count(),
            1,
            "re-install must not duplicate the managed block"
        );
        // Must still use CRLF after re-run on a CRLF file.
        assert!(
            !content2.contains("\r\r\n"),
            "re-install must not produce double-CR"
        );

        // SAFETY: restoring state while still holding the lock.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_data {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
            match prev_config {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }
}
