//! Filesystem and path utilities shared across the engine.
//!
//! These primitives back both the engine [config file](crate::config) and
//! [credential storage](crate::auth::storage): resolving the per-user base
//! directory, validating untrusted path components, and writing files
//! atomically with restrictive permissions. They are domain-agnostic so callers
//! that persist their own files can reuse them rather than re-implementing the
//! same path safety and atomic-write logic.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::CliCoreError;

/// Reads `key` from the environment as a non-empty path, or `None`.
fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// XDG-conventional `$HOME/.config`, if `HOME` is set.
fn home_config_dir() -> Option<PathBuf> {
    env_path("HOME").map(|home| home.join(".config"))
}

/// Resolves the per-user base directory for an app's config and data files.
///
/// Returns `$XDG_CONFIG_HOME` when set, else `$HOME/.config` (or `%APPDATA%` on
/// Windows). Only absolute paths are accepted; a relative value is rejected so
/// files never land relative to the current working directory.
#[must_use]
pub fn config_base_dir() -> Option<PathBuf> {
    env_path("XDG_CONFIG_HOME")
        .or_else(|| {
            // On Windows prefer APPDATA over HOME/.config: HOME is often set by
            // Git Bash/MSYS shells and would place files in a non-standard
            // location. On all other platforms prefer XDG-conventional
            // HOME/.config, falling back to APPDATA only as a last resort.
            // `cfg!(windows)` keeps both branches compiled (and type-checked)
            // on every platform.
            if cfg!(windows) {
                env_path("APPDATA").or_else(home_config_dir)
            } else {
                home_config_dir().or_else(|| env_path("APPDATA"))
            }
        })
        // Reject relative paths: a relative XDG_CONFIG_HOME/APPDATA/HOME would
        // silently place files relative to the current working directory.
        .filter(|p| p.is_absolute())
}

/// Returns true only when `s` is a single, non-traversal path component that is
/// valid on all supported platforms.
///
/// Use this to validate untrusted segments (app ids, environment names, etc.)
/// before joining them into a path.
///
/// Rejects:
/// - empty strings, `.`, and `..`
/// - strings containing `/` or `\` (path separators on any platform)
/// - Windows-forbidden filename characters: `:  * ? " < > |`
/// - ASCII control characters (bytes 0x00–0x1F) and the DEL character (0x7F)
/// - leading or trailing space (leading space is invisible in directory listings)
/// - trailing `.` (valid on Unix but rejected by Windows)
/// - Windows reserved device names (`CON`, `NUL`, `COM1`, etc.) with or without extension
#[must_use]
pub fn is_safe_path_component(s: &str) -> bool {
    // '/' is listed explicitly because Path::components() silently strips trailing
    // slashes — "prod/" parses as a single Normal("prod") component and would
    // otherwise pass the components check below.
    const FORBIDDEN: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    if s.contains(FORBIDDEN) || s.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return false;
    }
    if s.starts_with(' ') || s.ends_with('.') || s.ends_with(' ') {
        return false;
    }
    // Windows treats these device names as special regardless of extension,
    // e.g. opening "NUL.json" writes to the null device, not a file.
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8",
        "LPT9",
    ];
    let stem = Path::new(s)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(s);
    if RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
        return false;
    }
    let mut components = Path::new(s).components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}

/// Writes `contents` to `path` via a uniquely-named temp file then renames it
/// into place. On Unix the rename is atomic, the file is created `0600`, and the
/// parent directory is best-effort restricted to `0700`. On Windows the rename
/// replaces an existing destination but is not crash-atomic.
///
/// # Errors
/// Returns an error when the directory cannot be created or the write/rename
/// fails.
pub fn write_string_atomic(path: &Path, contents: &str) -> crate::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliCoreError::message(format!("failed to create directory: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if let Err(e) = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            {
                tracing::debug!(
                    path = %parent.display(),
                    error = %e,
                    "could not restrict directory permissions"
                );
            }
        }
    }
    // Unique temp name without pulling in `rand`: pid plus a monotonic counter is
    // unique within a process, and the pid differs across processes.
    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let tmp_path = path.with_file_name(format!(
        "{}.{pid:x}.{unique:x}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("tmp"),
    ));
    write_tmp_file(&tmp_path, contents)?;
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        std::fs::remove_file(&tmp_path).ok();
        return Err(CliCoreError::message(format!(
            "failed to finalize {}: {e}",
            path.display()
        )));
    }
    Ok(())
}

/// Opens `tmp_path` with `O_CREAT|O_EXCL` and writes `contents`, mode `0600` on
/// Unix so files are never world-readable.
fn write_tmp_file(tmp_path: &Path, contents: &str) -> crate::Result<()> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts.open(tmp_path).map_err(|e| {
        CliCoreError::message(format!("failed to write {}: {e}", tmp_path.display()))
    })?;
    file.write_all(contents.as_bytes())
        .map_err(|e| CliCoreError::message(format!("failed to write {}: {e}", tmp_path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_env::with_xdg_config_home;

    #[test]
    fn safe_path_component_basic() {
        assert!(is_safe_path_component("godaddy"));
        assert!(!is_safe_path_component(".."));
        assert!(!is_safe_path_component(""));
        assert!(!is_safe_path_component("a/b"));
        assert!(!is_safe_path_component("NUL"));
    }

    #[test]
    fn safe_path_component_rejects_windows_reserved_names() {
        for name in &[
            "CON", "con", "NUL", "nul", "COM1", "LPT9", "CON.txt", "NUL.json",
        ] {
            assert!(
                !is_safe_path_component(name),
                "{name:?} should be rejected as a Windows reserved name"
            );
        }
    }

    #[test]
    fn safe_path_component_rejects_control_and_space_edges() {
        assert!(!is_safe_path_component(" prod"), "leading space");
        assert!(!is_safe_path_component("prod\x7f"), "DEL byte");
        assert!(!is_safe_path_component("prod."), "trailing dot");
        assert!(!is_safe_path_component("prod "), "trailing space");
    }

    #[test]
    fn safe_path_component_accepts_normal_values() {
        for name in &["dev", "prod", "staging", "my-app", "my_app", "app.v2"] {
            assert!(is_safe_path_component(name), "{name:?} should be accepted");
        }
    }

    #[test]
    fn config_base_dir_rejects_relative_xdg() {
        with_xdg_config_home(Path::new("."), || {
            assert!(
                config_base_dir().is_none(),
                "relative XDG_CONFIG_HOME should be rejected"
            );
        });
    }

    #[test]
    fn config_base_dir_honors_xdg() {
        let dir = std::env::temp_dir().join("cli-engine-fs-base-test");
        with_xdg_config_home(&dir, || {
            assert_eq!(config_base_dir(), Some(dir.clone()));
        });
    }

    #[tokio::test]
    async fn write_string_atomic_round_trip_creates_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nested").join("file.txt");
        write_string_atomic(&path, "hello").expect("write");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "hello");
        // Overwrite replaces the contents.
        write_string_atomic(&path, "world").expect("rewrite");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "world");
        // No stray temp files remain alongside the target.
        let strays: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "temp files should be renamed away");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_string_atomic_sets_owner_only_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("secret.txt");
        write_string_atomic(&path, "s3cr3t").expect("write");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "file should be owner read/write only");
    }
}
