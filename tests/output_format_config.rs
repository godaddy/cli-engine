//! End-to-end coverage for default-output-format selection via config file,
//! env var, and the `--output`/`--human`/`--json` flags, exercised through
//! `Cli::run`.
//!
//! Mirrors `tests/credential_store_config.rs`: these tests mutate the
//! process-global `XDG_CONFIG_HOME` and `${PREFIX}_OUTPUT` env vars, so they
//! serialize on a shared lock.
#![allow(unsafe_code)]
// These tests serialize on a std Mutex and hold the guard across `.await` to keep
// process-global env mutations race-free; that is the intent, not a bug.
#![allow(clippy::await_holding_lock)]

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use cli_engine::{
    Cli, CliConfig, CommandResult, CommandSpec, GroupSpec, Module, RuntimeCommandSpec,
    RuntimeGroupSpec,
};
use serde_json::json;

const APP_ID: &str = "outfmt-itest";
const ENV_VAR: &str = "OUTFMT_ITEST_OUTPUT";

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

/// Writes `<xdg>/<app>/config.toml` with the given `[output].format`.
fn write_config(xdg: &Path, format: &str) {
    let dir = xdg.join(APP_ID);
    std::fs::create_dir_all(&dir).expect("create config dir");
    std::fs::write(
        dir.join("config.toml"),
        format!("[output]\nformat = \"{format}\"\n"),
    )
    .expect("write config");
}

fn build_cli() -> Cli {
    let module = Module::new("Widgets", |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("widget", "Manage widgets")).with_command(
            RuntimeCommandSpec::new(
                CommandSpec::new("list", "List widgets").no_auth(true),
                async |_credential, _args| Ok(CommandResult::new(json!([{ "name": "alpha" }]))),
            ),
        )
    });
    Cli::new(
        CliConfig::new(APP_ID, "Output-format config test CLI", APP_ID).with_modules(vec![module]),
    )
}

/// Human table output always has this row-count footer; JSON never does.
fn looks_like_human_output(rendered: &str) -> bool {
    rendered.contains("(1 rows)")
}

#[tokio::test]
async fn config_file_sets_default_format_without_a_flag() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = EnvGuard::set("XDG_CONFIG_HOME", Some(&dir.path().to_string_lossy()));
    let _env = EnvGuard::set(ENV_VAR, None);
    write_config(dir.path(), "human");

    let out = build_cli().run(["outfmt-itest", "widget", "list"]).await;

    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    assert!(
        looks_like_human_output(&out.rendered),
        "config file's [output].format = \"human\" should apply with no flag: {}",
        out.rendered
    );
}

#[tokio::test]
async fn env_var_overrides_config_format() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = EnvGuard::set("XDG_CONFIG_HOME", Some(&dir.path().to_string_lossy()));
    write_config(dir.path(), "human");
    let _env = EnvGuard::set(ENV_VAR, Some("json"));

    let out = build_cli().run(["outfmt-itest", "widget", "list"]).await;

    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    assert!(
        !looks_like_human_output(&out.rendered),
        "env override should win over the config file: {}",
        out.rendered
    );
    serde_json::from_str::<serde_json::Value>(&out.rendered).expect("valid json");
}

#[tokio::test]
async fn flag_overrides_config_and_env() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = EnvGuard::set("XDG_CONFIG_HOME", Some(&dir.path().to_string_lossy()));
    write_config(dir.path(), "json");
    let _env = EnvGuard::set(ENV_VAR, Some("json"));

    let out = build_cli()
        .run(["outfmt-itest", "widget", "list", "--human"])
        .await;

    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    assert!(
        looks_like_human_output(&out.rendered),
        "--human should win over both the env var and the config file: {}",
        out.rendered
    );
}

#[tokio::test]
async fn invalid_config_format_falls_through_to_tty_default() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = EnvGuard::set("XDG_CONFIG_HOME", Some(&dir.path().to_string_lossy()));
    let _env = EnvGuard::set(ENV_VAR, None);
    write_config(dir.path(), "yaml");

    let out = build_cli().run(["outfmt-itest", "widget", "list"]).await;

    // Not attached to a TTY in a test process, so an invalid config value
    // falls through to the JSON default rather than failing the command.
    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    serde_json::from_str::<serde_json::Value>(&out.rendered).expect("valid json");
}

#[tokio::test]
async fn conflicting_output_format_flags_error_instead_of_silently_picking_one() {
    // DEVEX-888 repro: `--json --human` together used to resolve silently
    // (human won); it must now be a usage error, end to end.
    //
    // Holds the lock even though it never mutates an env var itself:
    // `build_cli()` calls `Cli::new`, which reads `XDG_CONFIG_HOME` via
    // `ConfigFile::load` regardless, so it must still serialize against the
    // other tests in this file that mutate it.
    let _guard = lock();
    let out = build_cli()
        .run(["outfmt-itest", "widget", "list", "--json", "--human"])
        .await;

    assert_ne!(
        out.exit_code, 0,
        "conflicting format flags must fail: {}",
        out.rendered
    );
    assert!(
        !out.rendered.contains("(1 rows)"),
        "must not have silently run in human mode: {}",
        out.rendered
    );
    serde_json::from_str::<serde_json::Value>(&out.rendered)
        .expect_err("must not have silently run in json mode either");
}
