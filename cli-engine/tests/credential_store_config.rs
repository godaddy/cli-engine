//! End-to-end coverage for credential-store selection via config file, env var,
//! and the `--credential-store` flag, exercised through `Cli::run` with a real
//! `PkceAuthProvider`.
//!
//! These tests mutate process-global state (`XDG_CONFIG_HOME`, the
//! `ITEST_CREDENTIAL_STORE` env var, and the `--credential-store` flag latch),
//! so they serialize on a shared lock. The file-storage backend is the seam we
//! assert against: a credential file is read in `file`/`auto` modes but never
//! in explicit `keyring` mode — so "status shows logged in" cleanly
//! distinguishes which backend the engine selected without needing a keychain
//! daemon or a browser login.
#![cfg(feature = "pkce-auth")]
#![allow(unsafe_code)]
// These tests serialize on a std Mutex and hold the guard across `.await` to keep
// process-global env mutations race-free; that is the intent, not a bug.
#![allow(clippy::await_holding_lock)]

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use cli_engine::auth::pkce::PkceAuthProvider;
use cli_engine::{Cli, CliConfig};

const APP_ID: &str = "itest";
const ENV_VAR: &str = "ITEST_CREDENTIAL_STORE";

/// Serializes the process-global mutations these tests perform.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// RAII guard that restores an env var when dropped. Caller must hold [`lock`].
struct EnvGuard {
    key: String,
    prev: Option<String>,
}

impl EnvGuard {
    fn set(key: &str, value: Option<&str>) -> Self {
        let prev = std::env::var(key).ok();
        // SAFETY: caller holds ENV_LOCK for the guard's lifetime.
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        Self {
            key: key.to_owned(),
            prev,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: caller holds ENV_LOCK for the guard's lifetime.
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var(&self.key, v),
                None => std::env::remove_var(&self.key),
            }
        }
    }
}

/// Writes a non-expired credential file at the path `FileStorage` reads.
fn seed_credential_file(xdg: &Path) {
    let dir = xdg.join(APP_ID).join("credentials");
    std::fs::create_dir_all(&dir).expect("create credentials dir");
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        + 3600;
    let json = format!(
        "{{\"access_token\":\"itok\",\"expires_at\":{expires_at},\"refresh_token\":null,\"scopes\":[]}}"
    );
    std::fs::write(dir.join("primary-dev.json"), json).expect("write credential file");
}

/// Writes `<xdg>/<app>/config.toml` with the given `[credentials].store`.
fn write_config(xdg: &Path, store: &str) {
    let dir = xdg.join(APP_ID);
    std::fs::create_dir_all(&dir).expect("create config dir");
    std::fs::write(
        dir.join("config.toml"),
        format!("[credentials]\nstore = \"{store}\"\n"),
    )
    .expect("write config");
}

fn build_cli() -> Cli {
    let provider = Arc::new(
        PkceAuthProvider::new(
            "primary",
            "https://example.com/auth",
            "https://example.com/token",
            "client-id",
            &["openid"],
        )
        .with_app_id(APP_ID),
    );
    Cli::new(
        CliConfig::new(APP_ID, "Integration CLI", APP_ID)
            .with_auth_provider(provider)
            .with_default_auth_provider("primary"),
    )
}

#[tokio::test]
async fn config_file_selects_file_store() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = EnvGuard::set("XDG_CONFIG_HOME", Some(&dir.path().to_string_lossy()));
    let _env = EnvGuard::set(ENV_VAR, None);
    write_config(dir.path(), "file");
    seed_credential_file(dir.path());

    let out = build_cli()
        .run(["itest", "auth", "status", "--env", "dev"])
        .await;

    assert_eq!(out.exit_code, 0, "expected success, got: {}", out.rendered);
    assert!(
        out.rendered.contains("dev"),
        "status should report the env: {}",
        out.rendered
    );
    assert!(
        !out.rendered.contains("not logged in"),
        "file store should find the seeded credential: {}",
        out.rendered
    );
}

#[tokio::test]
async fn explicit_keyring_mode_ignores_credential_file() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = EnvGuard::set("XDG_CONFIG_HOME", Some(&dir.path().to_string_lossy()));
    let _env = EnvGuard::set(ENV_VAR, None);
    // Explicit keyring mode: a credential file exists but must be ignored
    // (keyring-only never reads the file).
    seed_credential_file(dir.path());

    let out = build_cli()
        .run([
            "itest",
            "--credential-store",
            "keyring",
            "auth",
            "status",
            "--env",
            "dev",
        ])
        .await;

    assert_ne!(out.exit_code, 0, "expected not-logged-in: {}", out.rendered);
    assert!(
        out.rendered.contains("not logged in"),
        "keyring mode must not read the credential file: {}",
        out.rendered
    );

    // Reset the flag latch for subsequent tests.
    let _reset = build_cli()
        .run(["itest", "auth", "status", "--env", "dev"])
        .await;
}

#[tokio::test]
async fn env_var_overrides_config() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = EnvGuard::set("XDG_CONFIG_HOME", Some(&dir.path().to_string_lossy()));
    // Config says keyring, env says file: env wins, so the file is read.
    write_config(dir.path(), "keyring");
    let _env = EnvGuard::set(ENV_VAR, Some("file"));
    seed_credential_file(dir.path());

    let out = build_cli()
        .run(["itest", "auth", "status", "--env", "dev"])
        .await;

    assert_eq!(
        out.exit_code, 0,
        "env override should win: {}",
        out.rendered
    );
    assert!(!out.rendered.contains("not logged in"), "{}", out.rendered);
}

#[tokio::test]
async fn flag_overrides_env() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = EnvGuard::set("XDG_CONFIG_HOME", Some(&dir.path().to_string_lossy()));
    // Env says keyring, flag says file: flag wins.
    let _env = EnvGuard::set(ENV_VAR, Some("keyring"));
    seed_credential_file(dir.path());

    let out = build_cli()
        .run([
            "itest",
            "--credential-store",
            "file",
            "auth",
            "status",
            "--env",
            "dev",
        ])
        .await;

    assert_eq!(
        out.exit_code, 0,
        "flag override should win: {}",
        out.rendered
    );
    assert!(!out.rendered.contains("not logged in"), "{}", out.rendered);

    // Reset the flag latch so later tests in this binary see no flag.
    let reset = build_cli()
        .run(["itest", "auth", "status", "--env", "dev"])
        .await;
    assert_ne!(reset.exit_code, 0, "{}", reset.rendered);
}

#[tokio::test]
async fn invalid_credential_store_flag_is_rejected() {
    let _guard = lock();
    let out = build_cli()
        .run([
            "itest",
            "--credential-store",
            "vault",
            "auth",
            "status",
            "--env",
            "dev",
        ])
        .await;
    assert_ne!(out.exit_code, 0, "invalid value should be a usage error");
    // Reset the flag latch for subsequent tests.
    let reset = build_cli()
        .run(["itest", "auth", "status", "--env", "dev"])
        .await;
    assert_ne!(reset.exit_code, 0, "{}", reset.rendered);
}
