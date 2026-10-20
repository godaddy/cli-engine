//! End-to-end coverage for a CLI-registered config schema: validated
//! `config set`, `unset`, discoverable `get`/`list`, and a handler reading the
//! same struct back. Mirrors `config_file.rs`'s per-test temp `XDG_CONFIG_HOME`.
#![allow(unsafe_code)]
#![allow(clippy::await_holding_lock)]

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use cli_engine::{
    Cli, CliConfig, CommandResult, CommandSpec, GroupSpec, Module, RuntimeCommandSpec,
    RuntimeGroupSpec,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const APP_ID: &str = "schematest";

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

struct EnvGuard {
    prev: Option<String>,
}

impl EnvGuard {
    fn set(path: &Path) -> Self {
        let prev = std::env::var("XDG_CONFIG_HOME").ok();
        // SAFETY: caller holds ENV_LOCK for the guard's lifetime.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", path) };
        Self { prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: caller holds ENV_LOCK for the guard's lifetime.
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }
}

/// The CLI's own settings.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
struct Prefs {
    /// Show domain names in their ASCII (punycode) form.
    punycode: bool,
    /// Rows per page.
    page_size: Option<u32>,
}

fn config_path(xdg: &Path) -> std::path::PathBuf {
    xdg.join(APP_ID).join("config.toml")
}

fn write_config(xdg: &Path, contents: &str) {
    std::fs::create_dir_all(xdg.join(APP_ID)).expect("mkdir");
    std::fs::write(config_path(xdg), contents).expect("write config");
}

/// A fresh CLI (reloading config) with `Prefs` registered and a `show` command
/// that reads them back through the typed API.
fn build_cli() -> Cli {
    let module = Module::new("Prefs", |_ctx| {
        RuntimeGroupSpec::new(GroupSpec::new("prefs", "Show prefs")).with_command(
            RuntimeCommandSpec::new_with_context(
                CommandSpec::new("show", "Show effective prefs")
                    .with_system("prefs")
                    .no_auth(true),
                async |ctx| {
                    let prefs: Prefs = ctx.config().deserialize()?;
                    Ok(CommandResult::new(json!({ "punycode": prefs.punycode })))
                },
            ),
        )
    });
    Cli::new(
        CliConfig::new(APP_ID, "Schema test CLI", APP_ID)
            .with_config_schema::<Prefs>()
            .with_module(module),
    )
}

async fn run(args: &[&str]) -> (i32, Value) {
    let mut argv = vec![APP_ID];
    argv.extend_from_slice(args);
    let out = build_cli().run(argv).await;
    let json = serde_json::from_str(&out.rendered).unwrap_or(Value::String(out.rendered));
    (out.exit_code, json)
}

#[tokio::test]
async fn set_get_unset_round_trip_with_typed_values() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(dir.path());

    let (code, out) = run(&["config", "set", "punycode", "true"]).await;
    assert_eq!(code, 0, "{out}");
    assert!(
        std::fs::read_to_string(config_path(dir.path()))
            .expect("written")
            .contains("punycode = true"),
        "stored as a TOML bool, not the string \"true\""
    );

    let (_, out) = run(&["config", "get", "punycode"]).await;
    let data = &out["data"];
    assert_eq!(data["value"], json!(true), "{out}");
    assert_eq!(data["source"], json!("file"));
    assert_eq!(data["default"], json!(false));
    assert!(
        data["description"]
            .as_str()
            .is_some_and(|d| d.contains("punycode")),
        "{out}"
    );

    let (_, out) = run(&["prefs", "show"]).await;
    assert_eq!(out["data"]["punycode"], json!(true), "{out}");

    let (code, out) = run(&["config", "unset", "punycode"]).await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["removed"], json!(true));
    let (_, out) = run(&["config", "get", "punycode"]).await;
    assert_eq!(out["data"]["value"], json!(false), "falls back to default");
    assert_eq!(out["data"]["source"], json!("default"));
}

#[tokio::test]
async fn set_rejects_bad_values_and_unknown_keys_with_a_fix() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(dir.path());

    for args in [
        ["config", "set", "punycode", "banana"],
        ["config", "set", "page_size", "-3"],
        ["config", "set", "punycod", "true"],
    ] {
        let (code, out) = run(&args).await;
        assert_ne!(code, 0, "{args:?} should fail: {out}");
        assert!(
            out["fix"].as_str().is_some_and(|f| !f.is_empty()),
            "{args:?} should carry a fix: {out}"
        );
    }
    assert!(
        !config_path(dir.path()).exists(),
        "rejected sets must not create the file"
    );

    let (_, out) = run(&["config", "set", "punycod", "true"]).await;
    assert!(
        out["fix"].as_str().unwrap_or_default().contains("punycode"),
        "{out}"
    );
}

#[tokio::test]
async fn list_shows_known_keys_defaults_and_undeclared_file_keys() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(dir.path());
    write_config(dir.path(), "stale_key = 1\n");

    let (code, out) = run(&["config", "list"]).await;
    assert_eq!(code, 0, "{out}");
    let rows = out["data"].as_array().expect("array of settings");
    let find = |key: &str| {
        rows.iter()
            .find(|r| r["key"] == key)
            .unwrap_or(&Value::Null)
    };
    assert_eq!(find("punycode")["source"], json!("default"));
    assert_eq!(find("page_size")["source"], json!("unset"));
    assert_eq!(find("credentials.store")["default"], json!("auto"));
    assert_eq!(find("stale_key")["source"], json!("file"), "{out}");
}

#[tokio::test]
async fn config_set_refuses_to_overwrite_a_malformed_file() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(dir.path());
    write_config(dir.path(), "not = valid = toml");

    let (code, out) = run(&["config", "set", "punycode", "true"]).await;
    assert_ne!(code, 0, "{out}");
    assert_eq!(
        std::fs::read_to_string(config_path(dir.path())).expect("still there"),
        "not = valid = toml",
        "the user's broken file must be left alone"
    );

    // A typed read surfaces the problem instead of silently using defaults.
    let (code, out) = run(&["prefs", "show"]).await;
    assert_ne!(code, 0, "{out}");
}

#[tokio::test]
async fn a_hand_edited_wrong_type_is_an_error_not_a_default() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(dir.path());
    write_config(dir.path(), "punycode = \"banana\"\n");

    let (code, out) = run(&["prefs", "show"]).await;
    assert_ne!(code, 0, "{out}");
}

#[tokio::test]
async fn invalid_engine_values_are_validation_errors_with_a_fix() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(dir.path());

    let (code, out) = run(&["config", "set", "credentials.store", "bogus"]).await;
    assert_ne!(code, 0, "{out}");
    assert_eq!(out["error"]["code"], json!("VALIDATION_ERROR"), "{out}");
    assert!(out["fix"].as_str().is_some_and(|f| !f.is_empty()), "{out}");
}

#[tokio::test]
async fn get_and_list_report_a_malformed_file_instead_of_showing_defaults() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(dir.path());
    write_config(dir.path(), "not = valid = toml");

    for args in [["config", "get", "punycode"], ["config", "list", "--json"]] {
        let (code, out) = run(&args).await;
        assert_ne!(
            code, 0,
            "{args:?} must not show defaults for a broken file: {out}"
        );
        assert!(
            out["fix"].as_str().is_some_and(|f| !f.is_empty()),
            "{args:?}: {out}"
        );
        assert!(
            out["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("config.toml")),
            "{args:?} should name the file: {out}"
        );
    }

    // `path` stays usable: it is how a user finds the file to fix.
    let (code, out) = run(&["config", "path"]).await;
    assert_eq!(code, 0, "{out}");
}
