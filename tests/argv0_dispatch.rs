//! Integration tests for busybox/git-style `argv[0]` dispatch.

use clap::Arg;
use cli_engine::{
    Argv0LinkMethod, BuildInfo, Cli, CliConfig, CommandResult, CommandSpec, GroupSpec, Module,
    RuntimeCommandSpec, RuntimeGroupSpec,
};
use serde_json::{Value, json};

/// A `project list` module mirroring the canonical consumer sample.
fn platform_module() -> Module {
    Module::new("Platform Systems", |_context| {
        RuntimeGroupSpec::new(GroupSpec::new("project", "Manage projects"))
            .with_command(list_projects())
    })
}

fn list_projects() -> RuntimeCommandSpec {
    RuntimeCommandSpec::new(
        CommandSpec::new("list", "List projects")
            .with_system("projects-api")
            .with_default_fields("id,name")
            .with_arg(Arg::new("team").long("team").required(true))
            .no_auth(true),
        async |_credential, args| {
            let team = args.get("team").and_then(Value::as_str).unwrap_or_default();
            Ok(CommandResult::new(json!([
                {"id": "p1", "name": format!("{team}-api")}
            ])))
        },
    )
}

/// An independent personality CLI with its own name, version, and command.
fn legacy_personality() -> CliConfig {
    CliConfig::new("legacy", "Legacy compatibility shim", "legacy")
        .with_build(BuildInfo::new("9.9.9"))
        .with_command(RuntimeCommandSpec::new(
            CommandSpec::new("ping", "Health check").no_auth(true),
            async |_credential, _args| Ok(CommandResult::new(json!({"pong": true}))),
        ))
}

/// Host CLI that registers both an alias and a personality route.
fn routed_cli() -> Cli {
    Cli::new(
        CliConfig::new("my-cli", "Team CLI", "my-cli")
            .with_build(BuildInfo::new("0.1.0"))
            .with_module(platform_module())
            .with_argv0_alias("pl", ["project", "list"])
            .with_argv0_personality("legacy", legacy_personality),
    )
}

/// Control CLI with no argv0 routes (proves zero behavior change).
fn plain_cli() -> Cli {
    Cli::new(
        CliConfig::new("my-cli", "Team CLI", "my-cli")
            .with_build(BuildInfo::new("0.1.0"))
            .with_module(platform_module()),
    )
}

#[tokio::test]
async fn alias_symlink_name_dispatches_to_command_path() {
    let cli = routed_cli();

    // Invoked through a symlinked path named `pl`.
    let out = cli.run(["/usr/local/bin/pl", "--team", "platform"]).await;
    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    assert_eq!(
        serde_json::from_str::<Value>(&out.rendered).expect("json"),
        json!({"data": [{"id": "p1", "name": "platform-api"}]})
    );
}

#[tokio::test]
async fn windows_executable_link_name_strips_extension() {
    let cli = routed_cli();

    // A Windows soft symlink or hard link to the binary is named `pl.exe`; the
    // OS sets argv[0] to that link name, and the `.exe` extension is stripped
    // before matching the registered route `pl`.
    let out = cli.run(["pl.exe", "--team", "platform"]).await;
    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    assert_eq!(
        serde_json::from_str::<Value>(&out.rendered).expect("json"),
        json!({"data": [{"id": "p1", "name": "platform-api"}]})
    );
}

#[tokio::test]
async fn alias_passes_global_flags_through() {
    let cli = routed_cli();

    let out = cli
        .run(["pl", "--team", "platform", "--output", "human"])
        .await;
    assert_eq!(out.exit_code, 0, "{}", out.rendered);
    assert!(out.rendered.contains("platform-api"), "{}", out.rendered);
}

#[tokio::test]
async fn alias_matches_equivalent_canonical_invocation() {
    let cli = routed_cli();

    let aliased = cli.run(["pl", "--team", "x"]).await;
    let canonical = cli.run(["my-cli", "project", "list", "--team", "x"]).await;
    assert_eq!(aliased, canonical);
}

#[tokio::test]
async fn personality_symlink_name_runs_separate_application() {
    let cli = routed_cli();

    let ping = cli.run(["legacy", "ping"]).await;
    assert_eq!(ping.exit_code, 0, "{}", ping.rendered);
    assert_eq!(
        serde_json::from_str::<Value>(&ping.rendered).expect("json"),
        json!({"data": {"pong": true}})
    );

    // Version and help reflect the personality, not the host.
    let version = cli.run(["legacy", "--version"]).await;
    assert_eq!(version.exit_code, 0, "{}", version.rendered);
    assert!(
        version.rendered.contains("legacy version 9.9.9"),
        "{}",
        version.rendered
    );
    assert!(!version.rendered.contains("my-cli"), "{}", version.rendered);

    let help = cli.run(["legacy", "--help"]).await;
    assert!(
        help.rendered.contains("Legacy compatibility shim"),
        "{}",
        help.rendered
    );
}

#[tokio::test]
async fn explicit_argv0_command_forces_alias_and_personality() {
    let cli = routed_cli();

    let aliased = cli.run(["my-cli", "argv0", "pl", "--team", "x"]).await;
    let canonical = cli.run(["my-cli", "project", "list", "--team", "x"]).await;
    assert_eq!(aliased, canonical);

    let personality = cli.run(["my-cli", "argv0", "legacy", "ping"]).await;
    assert_eq!(personality.exit_code, 0, "{}", personality.rendered);
    assert_eq!(
        serde_json::from_str::<Value>(&personality.rendered).expect("json"),
        json!({"data": {"pong": true}})
    );
}

#[tokio::test]
async fn explicit_argv0_name_strips_extension() {
    let cli = routed_cli();

    // A `.cmd` shim passing its full filename, or any caller appending `.exe`,
    // still matches the route registered under the bare name `pl` / `legacy`.
    let cmd_shim = cli.run(["my-cli", "argv0", "pl.cmd", "--team", "x"]).await;
    let canonical = cli.run(["my-cli", "project", "list", "--team", "x"]).await;
    assert_eq!(cmd_shim, canonical);

    let exe_link = cli.run(["my-cli", "argv0", "legacy.exe", "ping"]).await;
    assert_eq!(exe_link.exit_code, 0, "{}", exe_link.rendered);
    assert_eq!(
        serde_json::from_str::<Value>(&exe_link.rendered).expect("json"),
        json!({"data": {"pong": true}})
    );
}

#[tokio::test]
async fn explicit_argv0_with_unknown_name_errors() {
    let cli = routed_cli();

    let out = cli.run(["my-cli", "argv0", "bogus"]).await;
    assert_eq!(out.exit_code, 2, "{}", out.rendered);
    assert!(
        out.rendered.contains("not a registered argv0 name"),
        "{}",
        out.rendered
    );
    // Lists the known names so the caller can correct the invocation.
    assert!(out.rendered.contains("legacy"), "{}", out.rendered);
    assert!(out.rendered.contains("pl"), "{}", out.rendered);
}

#[tokio::test]
async fn bare_argv0_command_errors() {
    let cli = routed_cli();

    let out = cli.run(["my-cli", "argv0"]).await;
    assert_eq!(out.exit_code, 2, "{}", out.rendered);
    assert!(out.rendered.contains("requires a name"), "{}", out.rendered);
}

#[tokio::test]
async fn renamed_binary_falls_through_to_default_when_routes_registered() {
    let cli = routed_cli();

    // A binary renamed to an unregistered name still runs the default CLI.
    let renamed = cli
        .run(["otherthing", "project", "list", "--team", "x"])
        .await;
    let canonical = cli.run(["my-cli", "project", "list", "--team", "x"]).await;
    assert_eq!(renamed.exit_code, 0, "{}", renamed.rendered);
    assert_eq!(renamed, canonical);
}

#[tokio::test]
async fn no_routes_is_behaviorally_identical_to_today() {
    let plain = plain_cli();

    // Normal invocation is unaffected.
    let normal = plain
        .run(["my-cli", "project", "list", "--team", "x"])
        .await;
    assert_eq!(normal.exit_code, 0, "{}", normal.rendered);

    // With no routes, `argv0` is just an unrecognized subcommand (clap behavior),
    // not the meta-command.
    let meta = plain.run(["my-cli", "argv0", "pl"]).await;
    assert_ne!(meta.exit_code, 0);
    assert!(meta.rendered.contains("argv0"), "{}", meta.rendered);
    assert!(
        !meta.rendered.contains("not a registered argv0 name"),
        "{}",
        meta.rendered
    );
}

#[tokio::test]
async fn argv0_command_is_hidden_from_help_tree_and_search() {
    let cli = routed_cli();

    let help = cli.run(["my-cli", "--help"]).await;
    assert!(!help.rendered.contains("argv0"), "{}", help.rendered);

    let tree = cli.run(["my-cli", "tree"]).await;
    assert!(!tree.rendered.contains("argv0"), "{}", tree.rendered);

    let search = cli
        .run(["my-cli", "--search", "argv0", "--output", "json"])
        .await;
    assert!(
        !search.rendered.contains("\"argv0\""),
        "{}",
        search.rendered
    );
}

#[test]
fn argv0_names_lists_registered_routes() {
    let cli = routed_cli();
    // BTreeMap keys come back sorted.
    assert_eq!(cli.argv0_names(), vec!["legacy", "pl"]);
}

#[test]
fn create_link_rejects_unknown_name() {
    let cli = routed_cli();
    let dir = tempfile::tempdir().expect("tempdir");
    let err = cli
        .create_link("bogus", dir.path(), None, Argv0LinkMethod::SoftLink)
        .expect_err("unknown name should error");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn create_link_is_idempotent() {
    let cli = routed_cli();
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("my-cli-bin");
    std::fs::write(&target, b"binary").expect("write target");

    let first = cli
        .create_link("pl", dir.path(), Some(&target), Argv0LinkMethod::SoftLink)
        .expect("first create");
    // Re-running (e.g. self-healing) leaves the existing link and returns its path.
    let second = cli
        .create_link("pl", dir.path(), Some(&target), Argv0LinkMethod::SoftLink)
        .expect("second create");
    assert_eq!(first, second);
    assert_eq!(first.file_name().and_then(|n| n.to_str()), Some("pl"));
}

#[cfg(unix)]
#[test]
fn create_link_soft_link_points_at_target() {
    let cli = routed_cli();
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("my-cli-bin");
    std::fs::write(&target, b"binary").expect("write target");

    let link = cli
        .create_link("pl", dir.path(), Some(&target), Argv0LinkMethod::SoftLink)
        .expect("create soft link");
    let meta = std::fs::symlink_metadata(&link).expect("symlink metadata");
    assert!(meta.file_type().is_symlink(), "expected a symlink");
    assert_eq!(std::fs::read_link(&link).expect("read_link"), target);
}

#[cfg(unix)]
#[test]
fn create_link_hard_link_shares_target_inode() {
    use std::os::unix::fs::MetadataExt;

    let cli = routed_cli();
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("my-cli-bin");
    std::fs::write(&target, b"binary").expect("write target");

    let link = cli
        .create_link("pl", dir.path(), Some(&target), Argv0LinkMethod::HardLink)
        .expect("create hard link");
    let link_meta = std::fs::symlink_metadata(&link).expect("link metadata");
    assert!(
        !link_meta.file_type().is_symlink(),
        "hard link is not a symlink"
    );
    let target_meta = std::fs::metadata(&target).expect("target metadata");
    assert_eq!(link_meta.ino(), target_meta.ino(), "same inode");
}

#[cfg(unix)]
#[test]
fn create_link_script_is_executable_shim() {
    use std::os::unix::fs::PermissionsExt;

    let cli = routed_cli();
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("my-cli-bin");
    std::fs::write(&target, b"binary").expect("write target");

    let link = cli
        .create_link("pl", dir.path(), Some(&target), Argv0LinkMethod::Script)
        .expect("create script");
    assert_eq!(link.file_name().and_then(|n| n.to_str()), Some("pl"));
    let body = std::fs::read_to_string(&link).expect("read script");
    assert!(body.starts_with("#!/bin/sh"), "{body}");
    assert!(body.contains("argv0 pl"), "{body}");
    let mode = std::fs::metadata(&link)
        .expect("metadata")
        .permissions()
        .mode();
    assert!(mode & 0o111 != 0, "script should be executable: {mode:o}");
}

#[cfg(unix)]
#[test]
fn create_link_defaults_to_current_exe() {
    let cli = routed_cli();
    let dir = tempfile::tempdir().expect("tempdir");

    let link = cli
        .create_link("pl", dir.path(), None, Argv0LinkMethod::SoftLink)
        .expect("create link to current exe");
    let resolved = std::fs::read_link(&link).expect("read_link");
    assert_eq!(resolved, std::env::current_exe().expect("current_exe"));
}

#[test]
fn create_link_creates_missing_directory() {
    let cli = routed_cli();
    let root = tempfile::tempdir().expect("tempdir");
    let nested = root.path().join("does").join("not").join("exist");
    let target = root.path().join("target");
    std::fs::write(&target, b"binary").expect("write target");

    let link = cli
        .create_link("pl", &nested, Some(&target), Argv0LinkMethod::Script)
        .expect("create link in missing dir");
    assert!(nested.is_dir(), "directory should have been created");
    assert!(link.starts_with(&nested));
}

#[cfg(unix)]
#[test]
fn generated_unix_script_forwards_argv0_invocation() {
    use std::os::unix::fs::PermissionsExt;

    let cli = routed_cli();
    let dir = tempfile::tempdir().expect("tempdir");

    // A stand-in "binary" that echoes each argument it received on its own line,
    // so we can observe exactly what the generated shim forwards.
    let target = dir.path().join("fake-bin");
    std::fs::write(
        &target,
        "#!/bin/sh\nfor arg in \"$@\"; do echo \"$arg\"; done\n",
    )
    .expect("write target");
    let mut perms = std::fs::metadata(&target).expect("meta").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&target, perms).expect("chmod target");

    let shim = cli
        .create_link("pl", dir.path(), Some(&target), Argv0LinkMethod::Script)
        .expect("create script");

    // Running the shim must invoke the target as `<target> argv0 pl --team x`.
    let output = std::process::Command::new(&shim)
        .args(["--team", "x"])
        .output()
        .expect("run shim");
    assert!(output.status.success(), "shim exit: {:?}", output.status);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "argv0\npl\n--team\nx\n",
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(windows)]
#[test]
fn create_link_soft_link_uses_exe_extension() {
    let cli = routed_cli();
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("my-cli.exe");
    std::fs::write(&target, b"binary").expect("write target");

    // Windows symlink creation needs Developer Mode or elevation; skip cleanly
    // when the host CI lacks the privilege rather than failing the suite.
    let link = match cli.create_link("pl", dir.path(), Some(&target), Argv0LinkMethod::SoftLink) {
        Ok(link) => link,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(err) => panic!("unexpected error creating symlink: {err}"),
    };
    assert_eq!(link.file_name().and_then(|n| n.to_str()), Some("pl.exe"));
    let meta = std::fs::symlink_metadata(&link).expect("symlink metadata");
    assert!(meta.file_type().is_symlink(), "expected a symlink");
    assert_eq!(std::fs::read_link(&link).expect("read_link"), target);
}

#[cfg(windows)]
#[test]
fn create_link_hard_link_uses_exe_extension() {
    let cli = routed_cli();
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("my-cli.exe");
    std::fs::write(&target, b"binary-contents").expect("write target");

    let link = cli
        .create_link("pl", dir.path(), Some(&target), Argv0LinkMethod::HardLink)
        .expect("create hard link");
    assert_eq!(link.file_name().and_then(|n| n.to_str()), Some("pl.exe"));
    let meta = std::fs::symlink_metadata(&link).expect("link metadata");
    assert!(!meta.file_type().is_symlink(), "hard link is not a symlink");
    assert_eq!(
        std::fs::read(&link).expect("read link"),
        std::fs::read(&target).expect("read target"),
        "hard link sees the target contents"
    );
}

#[cfg(windows)]
#[test]
fn create_link_script_is_cmd_shim() {
    let cli = routed_cli();
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("my-cli.exe");
    std::fs::write(&target, b"binary").expect("write target");

    let link = cli
        .create_link("pl", dir.path(), Some(&target), Argv0LinkMethod::Script)
        .expect("create cmd shim");
    assert_eq!(link.file_name().and_then(|n| n.to_str()), Some("pl.cmd"));
    let body = std::fs::read_to_string(&link).expect("read shim");
    assert!(body.starts_with("@\""), "{body}");
    assert!(body.contains("argv0 pl %*"), "{body}");
}

#[cfg(windows)]
#[test]
fn generated_windows_cmd_forwards_argv0_invocation() {
    let cli = routed_cli();
    let dir = tempfile::tempdir().expect("tempdir");

    // A stand-in "binary" (batch file) that echoes the arguments it received.
    let target = dir.path().join("fake-bin.cmd");
    std::fs::write(&target, "@echo %*\r\n").expect("write target");

    let shim = cli
        .create_link("pl", dir.path(), Some(&target), Argv0LinkMethod::Script)
        .expect("create cmd shim");

    // Batch files are launched through cmd.exe; the shim must invoke the target
    // as `<target> argv0 pl --team x`.
    let output = std::process::Command::new("cmd")
        .arg("/C")
        .arg(&shim)
        .args(["--team", "x"])
        .output()
        .expect("run shim");
    assert!(output.status.success(), "shim exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("argv0 pl"), "{stdout}");
    assert!(stdout.contains("--team x"), "{stdout}");
}
