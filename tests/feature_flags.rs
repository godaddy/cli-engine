//! End-to-end coverage for the feature-flagging system, driven entirely through
//! [`Cli::run`] the way a real consumer binary would: cascading resolution across
//! module/group/command with an override, pruning a hidden node from `--help` and
//! `--schema` (and the unknown-command error on direct invocation), environment
//! `min_stage`/`feature_overrides` layering (compiled and via an `<ENV>_*` env
//! var), and the built-in `flags list`/`flags info` introspection commands.
//!
//! Cascading resolution, pruning internals, and env-var precedence already have
//! thorough unit coverage in `src/cli.rs` and `src/environments.rs`; this file
//! proves the same mechanism end to end through the only surface a real consumer
//! CLI uses, rather than re-testing those internals directly.
#![allow(unsafe_code)]
// The env-var test holds `ENV_LOCK` across an `.await` on purpose, to keep the
// mutation race-free for the whole `Cli::new` + `Cli::run` sequence.
#![allow(clippy::await_holding_lock)]

use std::sync::{Arc, Mutex, MutexGuard};

use cli_engine::{
    Cli, CliConfig, CommandResult, CommandSpec, EnvironmentDef, Environments, GroupSpec, Module,
    RuntimeCommandSpec, RuntimeGroupSpec, Stage,
};
use serde_json::{Value, json};

/// A no-op, unauthenticated leaf command used to populate the fixture trees.
fn trivial_command(name: &'static str) -> RuntimeCommandSpec {
    RuntimeCommandSpec::new(
        CommandSpec::new(name, "Fixture command").no_auth(true),
        async |_credential, _args| Ok(CommandResult::new(json!({ "ok": true }))),
    )
}

/// Builds a `devkit` module: an unflagged top-level group with an unflagged
/// `status` command, and a nested `sandbox` group flagged Experimental whose
/// `peek` command has no flag of its own (so it inherits `sandbox`'s flag).
///
/// `status` is a sibling of the flagged `sandbox` group (not of `peek`): per the
/// engine's pruning semantics an invisible ancestor drops its whole subtree
/// unconditionally, so `peek` could never survive as a *direct* sibling of an
/// inherited-and-hidden command under the same flagged parent. Nesting the flag
/// one level below `devkit` is what lets `status` demonstrate an unrelated,
/// always-visible command surviving right next to a fully pruned subtree.
fn gated_module() -> Module {
    Module::new("Feature Flag Fixtures", |_ctx| {
        RuntimeGroupSpec::new(GroupSpec::new("devkit", "Devkit commands"))
            .with_group(
                RuntimeGroupSpec::new(
                    GroupSpec::new("sandbox", "Experimental sandbox tools")
                        .with_feature_flag("sandbox-flag", Stage::Experimental),
                )
                .with_command(trivial_command("peek")),
            )
            .with_command(trivial_command("status"))
    })
}

#[tokio::test]
async fn cascading_flag_prunes_inherited_descendant_but_keeps_unflagged_sibling() {
    // Default policy: `CliConfig` leaves `min_stage` at its `Stage::Ga` default,
    // so the Experimental `sandbox` subgroup (and everything it cascades into)
    // must be pruned, while the unrelated `status` command stays.
    let cli = Cli::new(
        CliConfig::new("flagcascade", "Flag cascade test", "flagcascade-app")
            .with_module(gated_module()),
    );

    let help = cli.run(["flagcascade", "devkit"]).await;
    assert_eq!(help.exit_code, 0, "{}", help.rendered);
    assert!(help.rendered.contains("status"), "{}", help.rendered);
    assert!(!help.rendered.contains("sandbox"), "{}", help.rendered);

    // The pruned command was never mounted into the clap tree, so dispatching
    // it directly fails the same way a typo'd command would.
    let dispatch = cli.run(["flagcascade", "devkit", "sandbox", "peek"]).await;
    assert_ne!(dispatch.exit_code, 0, "{}", dispatch.rendered);
    assert!(
        dispatch.rendered.contains("unknown command"),
        "{}",
        dispatch.rendered
    );

    // `--schema` does not resurrect it either: the path still resolves to
    // "unknown command", not a schema (or no-schema) envelope.
    let schema = cli
        .run([
            "flagcascade",
            "devkit",
            "sandbox",
            "peek",
            "--schema",
            "--output",
            "json",
        ])
        .await;
    assert_ne!(schema.exit_code, 0, "{}", schema.rendered);
    assert!(
        schema.rendered.contains("unknown command"),
        "{}",
        schema.rendered
    );

    // The unflagged sibling dispatches normally.
    let visible = cli
        .run(["flagcascade", "devkit", "status", "--output", "json"])
        .await;
    assert_eq!(visible.exit_code, 0, "{}", visible.rendered);
    let visible_json: Value = serde_json::from_str(&visible.rendered).expect("json output");
    assert_eq!(visible_json["data"]["ok"], true);
}

#[tokio::test]
async fn permissive_min_stage_reveals_previously_pruned_subtree() {
    // Same tree, but the consumer opts into Experimental visibility, so the
    // subgroup and its inheriting command are mounted and dispatchable.
    let cli = Cli::new(
        CliConfig::new(
            "flagpermissive",
            "Flag permissive test",
            "flagpermissive-app",
        )
        .with_min_stage(Stage::Experimental)
        .with_module(gated_module()),
    );

    let help = cli.run(["flagpermissive", "devkit"]).await;
    assert_eq!(help.exit_code, 0, "{}", help.rendered);
    assert!(help.rendered.contains("sandbox"), "{}", help.rendered);

    let dispatch = cli
        .run([
            "flagpermissive",
            "devkit",
            "sandbox",
            "peek",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(dispatch.exit_code, 0, "{}", dispatch.rendered);
}

#[tokio::test]
async fn environment_min_stage_loosens_consumer_policy_end_to_end() {
    // The `CliConfig` itself stays at its Ga default; only the active
    // ("flagtest-envmin") environment's compiled `min_stage` loosens visibility.
    // Proves the environment layer reaches pruning through the full `Cli::new` +
    // `Cli::run` pipeline, not just `Environments::resolve` in isolation.
    //
    // The environment name is deliberately test-scoped (not a real name like
    // "prod") so its derived `FLAGTEST_ENVMIN_MIN_STAGE` env var cannot collide
    // with an `<ENV>_MIN_STAGE` a developer/CI might have set for a real
    // environment, which would otherwise silently override the compiled
    // `min_stage` this test asserts on. This test therefore needs no `ENV_LOCK`.
    let cli = Cli::new(
        CliConfig::new("flagenv", "Flag environment test", "flagenv-app")
            .with_environments(Arc::new(
                Environments::new("flagtest-envmin").with_environment(
                    "flagtest-envmin",
                    EnvironmentDef::new().with_min_stage(Stage::Experimental),
                ),
            ))
            .with_module(gated_module()),
    );

    let help = cli.run(["flagenv", "devkit"]).await;
    assert_eq!(help.exit_code, 0, "{}", help.rendered);
    assert!(help.rendered.contains("sandbox"), "{}", help.rendered);

    let dispatch = cli
        .run(["flagenv", "devkit", "sandbox", "peek", "--output", "json"])
        .await;
    assert_eq!(dispatch.exit_code, 0, "{}", dispatch.rendered);
}

#[tokio::test]
async fn environment_feature_override_reveals_pruned_subtree_end_to_end() {
    // Distinct from the environment `min_stage` layer above: here both the
    // `CliConfig` and the environment leave `min_stage` at Ga, and it is the
    // environment's compiled per-key `feature_overrides` entry (forcing
    // `sandbox-flag` to Ga) that lifts the otherwise-Experimental subgroup into
    // visibility. Proves the environment feature-override layer (not just
    // `min_stage`) reaches pruning through the full `Cli::new` + `Cli::run`
    // pipeline.
    //
    // The environment name is test-scoped for the same collision reason as the
    // `min_stage` test above: its derived `FLAGTEST_FEATOVERRIDE_*` env vars
    // cannot clash with a real environment's, so no `ENV_LOCK` is needed.
    let cli = Cli::new(
        CliConfig::new(
            "flagenvoverride",
            "Flag environment override test",
            "flagenvoverride-app",
        )
        .with_environments(Arc::new(
            Environments::new("flagtest-featoverride").with_environment(
                "flagtest-featoverride",
                EnvironmentDef::new().with_feature_override("sandbox-flag", Stage::Ga),
            ),
        ))
        .with_module(gated_module()),
    );

    let help = cli.run(["flagenvoverride", "devkit"]).await;
    assert_eq!(help.exit_code, 0, "{}", help.rendered);
    assert!(help.rendered.contains("sandbox"), "{}", help.rendered);

    let dispatch = cli
        .run([
            "flagenvoverride",
            "devkit",
            "sandbox",
            "peek",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(dispatch.exit_code, 0, "{}", dispatch.rendered);
}

#[tokio::test]
async fn environment_min_stage_tightens_permissive_consumer_policy_end_to_end() {
    // The tightening direction, opposite every test above: the `CliConfig` is
    // *permissive* (`min_stage` = Experimental, which on its own reveals the
    // Experimental `sandbox` subgroup), but the active environment raises
    // `min_stage` back up to Ga and re-hides it. Proves the environment layer's
    // unconditional replace in `Cli::new` can strengthen — not only loosen — the
    // consumer's compiled policy, all the way through pruning and dispatch.
    //
    // Compiled-only environment, so no env var and no `ENV_LOCK`; the name is
    // test-scoped for the same collision reason as the loosening tests above.
    let cli = Cli::new(
        CliConfig::new("flagtighten", "Flag tighten test", "flagtighten-app")
            .with_min_stage(Stage::Experimental)
            .with_environments(Arc::new(
                Environments::new("flagtest-tighten").with_environment(
                    "flagtest-tighten",
                    EnvironmentDef::new().with_min_stage(Stage::Ga),
                ),
            ))
            .with_module(gated_module()),
    );

    // `sandbox` would be visible under the consumer's Experimental floor alone,
    // but the environment's Ga floor prunes it again.
    let help = cli.run(["flagtighten", "devkit"]).await;
    assert_eq!(help.exit_code, 0, "{}", help.rendered);
    assert!(help.rendered.contains("status"), "{}", help.rendered);
    assert!(!help.rendered.contains("sandbox"), "{}", help.rendered);

    // And the re-hidden node is not dispatchable: it was never mounted.
    let dispatch = cli
        .run([
            "flagtighten",
            "devkit",
            "sandbox",
            "peek",
            "--output",
            "json",
        ])
        .await;
    assert_ne!(dispatch.exit_code, 0, "{}", dispatch.rendered);
    assert!(
        dispatch.rendered.contains("unknown command"),
        "{}",
        dispatch.rendered
    );
}

/// Serializes this file's env-var mutations across parallel test threads.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// RAII guard that restores (or removes) an env var on drop, even on panic.
struct EnvGuard {
    key: &'static str,
    prev: Option<String>,
}

impl EnvGuard {
    /// Sets `key` to `value`. Caller must hold [`ENV_LOCK`] for the guard's
    /// entire lifetime.
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var(key).ok();
        // SAFETY: serialized by ENV_LOCK; guard restores/removes on any exit
        // incl. panic.
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: serialized by ENV_LOCK; guard restores/removes on any exit
        // incl. panic.
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

// A distinctive, test-scoped environment name so its derived env-var prefix
// (`FLAGTEST_ENVVAR_*`) cannot collide with a real environment name a developer
// might have configured locally.
const ENV_VAR_ENV_NAME: &str = "flagtest-envvar";
const ENV_VAR_MIN_STAGE: &str = "FLAGTEST_ENVVAR_MIN_STAGE";

#[tokio::test]
async fn env_var_min_stage_override_reaches_dispatch_end_to_end() {
    // Confirms the full `<ENV>_MIN_STAGE` env var -> `Environments::resolve` ->
    // `FlagPolicy` -> tree-pruning -> clap chain, not just that `resolve()`
    // returns the right `Environment` in isolation (already covered in
    // `src/environments.rs`'s unit tests).
    let _lock = lock();
    let _guard = EnvGuard::set(ENV_VAR_MIN_STAGE, "experimental");

    let cli = Cli::new(
        CliConfig::new("flagenvvar", "Flag env-var test", "flagenvvar-app")
            .with_environments(Arc::new(
                Environments::new(ENV_VAR_ENV_NAME)
                    .with_environment(ENV_VAR_ENV_NAME, EnvironmentDef::new()),
            ))
            .with_module(gated_module()),
    );

    let help = cli.run(["flagenvvar", "devkit"]).await;
    assert_eq!(help.exit_code, 0, "{}", help.rendered);
    assert!(help.rendered.contains("sandbox"), "{}", help.rendered);

    let dispatch = cli
        .run([
            "flagenvvar",
            "devkit",
            "sandbox",
            "peek",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(dispatch.exit_code, 0, "{}", dispatch.rendered);
}

/// Builds a module whose group carries its own feature flag (so it, and its
/// unflagged `list` command, both cascade to `key`/`stage`).
fn flagged_module(group_name: &'static str, key: &'static str, stage: Stage) -> Module {
    Module::new("Flags Introspection Fixtures", move |_ctx| {
        RuntimeGroupSpec::new(
            GroupSpec::new(group_name, "Introspection fixture group").with_feature_flag(key, stage),
        )
        .with_command(trivial_command("list"))
    })
}

#[tokio::test]
async fn flags_list_and_info_report_override_and_min_stage_decisions_end_to_end() {
    // One flag key is forced visible by a consumer-level override despite its
    // own Experimental declaration; the other relies solely on `min_stage`.
    // `flags list`/`flags info` should distinguish the two via `decided_by`.
    let cli = Cli::new(
        CliConfig::new("flagintro", "Flags introspection test", "flagintro-app")
            .with_min_stage(Stage::Beta)
            .with_feature_override("override-key", Stage::Ga)
            .with_module(flagged_module(
                "override-group",
                "override-key",
                Stage::Experimental,
            ))
            .with_module(flagged_module(
                "min-stage-group",
                "min-stage-key",
                Stage::Beta,
            )),
    );

    let list = cli
        .run(["flagintro", "flags", "list", "--output", "json"])
        .await;
    assert_eq!(list.exit_code, 0, "{}", list.rendered);
    let rendered: Value = serde_json::from_str(&list.rendered).expect("json output");
    let entries = rendered["data"].as_array().expect("data should be array");

    let override_entry = entries
        .iter()
        .find(|entry| entry["path"] == "override-group:list")
        .expect("override-decided command entry present");
    assert_eq!(override_entry["key"], "override-key");
    assert_eq!(override_entry["stage"], "experimental");
    assert_eq!(override_entry["visible"], true);

    let min_stage_entry = entries
        .iter()
        .find(|entry| entry["path"] == "min-stage-group:list")
        .expect("min-stage-decided command entry present");
    assert_eq!(min_stage_entry["key"], "min-stage-key");
    assert_eq!(min_stage_entry["stage"], "beta");
    assert_eq!(min_stage_entry["visible"], true);

    let override_info = cli
        .run([
            "flagintro",
            "flags",
            "info",
            "override-key",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(override_info.exit_code, 0, "{}", override_info.rendered);
    let override_data: Value = serde_json::from_str(&override_info.rendered).expect("json output");
    assert_eq!(override_data["data"]["policy"]["override"], "ga");
    assert!(
        override_data["data"]["entries"]
            .as_array()
            .expect("entries should be array")
            .iter()
            .all(|entry| entry["decided_by"] == "override")
    );

    let min_stage_info = cli
        .run([
            "flagintro",
            "flags",
            "info",
            "min-stage-key",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(min_stage_info.exit_code, 0, "{}", min_stage_info.rendered);
    let min_stage_data: Value =
        serde_json::from_str(&min_stage_info.rendered).expect("json output");
    assert!(min_stage_data["data"]["policy"]["override"].is_null());
    assert!(
        min_stage_data["data"]["entries"]
            .as_array()
            .expect("entries should be array")
            .iter()
            .all(|entry| entry["decided_by"] == "min_stage")
    );

    let unknown = cli.run(["flagintro", "flags", "info", "no-such-key"]).await;
    assert_ne!(unknown.exit_code, 0, "{}", unknown.rendered);
    assert!(
        unknown.rendered.contains("no such flag"),
        "{}",
        unknown.rendered
    );
}

#[tokio::test]
async fn flags_info_decides_per_entry_when_multiple_nodes_share_a_key() {
    // Two unrelated nodes declare the same key with different stages. The
    // override (to Beta) flips one node's outcome (Experimental -> visible)
    // but not the other's (Beta was already >= min_stage). `decided_by` must
    // reflect that per entry, not uniformly for the whole key.
    let cli = Cli::new(
        CliConfig::new("flagintro2", "Flags introspection test", "flagintro2-app")
            .with_min_stage(Stage::Beta)
            .with_feature_override("shared-key", Stage::Beta)
            .with_module(flagged_module(
                "already-visible-group",
                "shared-key",
                Stage::Beta,
            ))
            .with_module(flagged_module(
                "flipped-group",
                "shared-key",
                Stage::Experimental,
            )),
    );

    let info = cli
        .run([
            "flagintro2",
            "flags",
            "info",
            "shared-key",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(info.exit_code, 0, "{}", info.rendered);
    let data: Value = serde_json::from_str(&info.rendered).expect("json output");
    let entries = data["data"]["entries"].as_array().expect("entries array");

    let already_visible = entries
        .iter()
        .find(|entry| entry["path"] == "already-visible-group:list")
        .expect("already-visible entry present");
    assert_eq!(already_visible["visible"], true);
    assert_eq!(already_visible["decided_by"], "min_stage");

    let flipped = entries
        .iter()
        .find(|entry| entry["path"] == "flipped-group:list")
        .expect("flipped entry present");
    assert_eq!(flipped["visible"], true);
    assert_eq!(flipped["decided_by"], "override");
}
