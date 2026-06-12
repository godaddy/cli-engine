//! End-to-end coverage for the per-CLI config file: a consumer command reading
//! its own section, and the built-in `config get/set/path` group.
//!
//! Each test serializes on a shared lock and points `XDG_CONFIG_HOME` at a temp
//! dir. A fresh `Cli` is built per `run` so each invocation reloads the file from
//! disk, matching how a real one-shot CLI process behaves.
#![allow(unsafe_code)]
#![allow(clippy::await_holding_lock)]

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use cli_engine::{
    Cli, CliConfig, CommandResult, CommandSpec, GroupSpec, Module, RuntimeCommandSpec,
    RuntimeGroupSpec,
};
use serde::Deserialize;
use serde_json::json;

const APP_ID: &str = "cfgtest";

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

#[derive(Debug, Deserialize)]
struct Deploy {
    region: String,
}

fn write_config(xdg: &Path, contents: &str) {
    let dir = xdg.join(APP_ID);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("config.toml"), contents).expect("write config");
}

fn config_contents(xdg: &Path) -> Option<String> {
    std::fs::read_to_string(xdg.join(APP_ID).join("config.toml")).ok()
}

/// Builds a fresh CLI (reloading config from disk) with a consumer `deploy show`
/// command and the built-in `config` group mounted.
fn build_cli() -> Cli {
    let module = Module::new("Deploy", |_ctx| {
        RuntimeGroupSpec::new(GroupSpec::new("deploy", "Deploy things")).with_command(
            RuntimeCommandSpec::new_with_context(
                CommandSpec::new("show", "Show configured region")
                    .with_system("deploy")
                    .no_auth(true),
                async |ctx| {
                    let region = ctx
                        .config()
                        .section::<Deploy>("deploy")?
                        .map(|d| d.region)
                        .unwrap_or_else(|| "<none>".to_owned());
                    Ok(CommandResult::new(json!({ "region": region })))
                },
            ),
        )
    });
    Cli::new(
        CliConfig::new(APP_ID, "Config test CLI", APP_ID)
            .with_config_commands()
            .with_module(module),
    )
}

#[tokio::test]
async fn handler_reads_consumer_section() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(dir.path());
    write_config(dir.path(), "[deploy]\nregion = \"us-west\"\n");

    let out = build_cli().run(["cfgtest", "deploy", "show"]).await;
    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    assert!(
        out.rendered.contains("us-west"),
        "handler should read [deploy].region: {}",
        out.rendered
    );
}

#[tokio::test]
async fn config_set_then_get_roundtrips_and_preserves_other_tables() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(dir.path());
    write_config(
        dir.path(),
        "# keep this comment\n[credentials]\nstore = \"file\"\n",
    );

    let set = build_cli()
        .run(["cfgtest", "config", "set", "deploy.region", "eu-west"])
        .await;
    assert_eq!(set.exit_code, 0, "{}", set.rendered);

    // Fresh CLI reloads from disk and sees the new value.
    let get = build_cli()
        .run(["cfgtest", "config", "get", "deploy.region"])
        .await;
    assert_eq!(get.exit_code, 0, "{}", get.rendered);
    assert!(get.rendered.contains("eu-west"), "{}", get.rendered);

    // The engine table and the comment survived the write.
    let on_disk = config_contents(dir.path()).expect("file exists");
    assert!(on_disk.contains("store = \"file\""), "{on_disk}");
    assert!(on_disk.contains("# keep this comment"), "{on_disk}");

    // Engine still reads its reserved key.
    let store = build_cli()
        .run(["cfgtest", "config", "get", "credentials.store"])
        .await;
    assert!(store.rendered.contains("file"), "{}", store.rendered);
}

#[tokio::test]
async fn config_set_dry_run_does_not_write() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(dir.path());
    // No config file exists yet.

    let out = build_cli()
        .run([
            "cfgtest",
            "config",
            "set",
            "deploy.region",
            "eu-west",
            "--dry-run",
        ])
        .await;
    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    assert!(
        config_contents(dir.path()).is_none(),
        "dry-run must not create the config file"
    );
}

#[tokio::test]
async fn config_path_prints_path() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(dir.path());

    let out = build_cli().run(["cfgtest", "config", "path"]).await;
    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    assert!(out.rendered.contains("cfgtest"), "{}", out.rendered);
    assert!(out.rendered.contains("config.toml"), "{}", out.rendered);
}

#[tokio::test]
async fn config_set_rejects_invalid_engine_value() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(dir.path());

    let out = build_cli()
        .run(["cfgtest", "config", "set", "credentials.store", "bogus"])
        .await;
    assert_ne!(
        out.exit_code, 0,
        "invalid engine value should fail: {}",
        out.rendered
    );
    assert!(
        config_contents(dir.path()).is_none(),
        "rejected set must not write the file"
    );
}
