/// Integration tests for the completion built-in via `Cli::run`.
///
/// These tests exercise the full dispatch path rather than the internal
/// `completion::generate_script` / `completion::install` functions directly.
/// They assert:
///   - every supported shell prints a raw script (no JSON envelope) that
///     references the binary name
///   - `$SHELL` auto-detection picks the correct shell
///   - `--install` writes the expected files under a TempDir HOME
///   - a second `--install` run is idempotent (single managed block)
///   - an unknown shell name exits non-zero without panicking
#[allow(clippy::unwrap_used)]
#[allow(clippy::await_holding_lock)]
mod completion_integration {
    use std::sync::{Mutex, MutexGuard};

    use cli_engine::{BuildInfo, Cli, CliConfig, GroupSpec, Module, RuntimeGroupSpec};
    use serde_json::Value;
    use tempfile::TempDir;

    // ---------------------------------------------------------------------------
    // Env mutation lock – integration tests cannot reach the crate-internal
    // `config::test_env::lock()`, so we define our own equivalent mutex here.
    // All tests that mutate HOME / XDG_* must hold this guard for their entire
    // duration (including across `.await` points).
    // ---------------------------------------------------------------------------
    static INSTALL_MUTEX: Mutex<()> = Mutex::new(());

    fn env_lock() -> MutexGuard<'static, ()> {
        INSTALL_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// RAII guard that sets an env var and restores its prior value on drop,
    /// so a panicking assertion cannot leak state into other tests.
    struct EnvVarGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvVarGuard {
        #[allow(unsafe_code)]
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            // SAFETY: every caller holds INSTALL_MUTEX for the guard's lifetime.
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: the INSTALL_MUTEX guard outlives every EnvVarGuard.
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Minimal test CLI – no real commands needed; completion generates scripts
    // from the clap Command tree, which just needs the binary to exist.
    // ---------------------------------------------------------------------------
    fn demo_cli() -> Cli {
        Cli::new(
            CliConfig::new("demo", "Demo CLI for completion tests", "demo")
                .with_build(BuildInfo::new("0.1.0"))
                .with_module(Module::new("Demo", |_ctx| {
                    RuntimeGroupSpec::new(GroupSpec::new("widget", "Manage widgets"))
                })),
        )
    }

    // =========================================================================
    // (a) Print tests: each supported shell exits 0 with a raw script containing
    //     the binary name and no JSON envelope.
    // =========================================================================

    #[tokio::test]
    async fn completion_print_bash_is_raw_script() {
        let cli = demo_cli();
        let out = cli.run(["demo", "completion", "bash"]).await;
        assert_eq!(out.exit_code, 0, "bash: {}", out.rendered);
        assert!(!out.rendered.is_empty(), "bash script should be non-empty");
        assert!(
            out.rendered.contains("demo"),
            "bash script should mention bin name; got: {}",
            out.rendered
        );
        // Must NOT be a JSON envelope.
        assert!(
            serde_json::from_str::<Value>(&out.rendered).is_err(),
            "bash output must not be a JSON envelope; got: {}",
            out.rendered
        );
    }

    #[tokio::test]
    async fn completion_print_zsh_is_raw_script() {
        let cli = demo_cli();
        let out = cli.run(["demo", "completion", "zsh"]).await;
        assert_eq!(out.exit_code, 0, "zsh: {}", out.rendered);
        assert!(!out.rendered.is_empty(), "zsh script should be non-empty");
        assert!(
            out.rendered.contains("demo"),
            "zsh script should mention bin name; got: {}",
            out.rendered
        );
        assert!(
            serde_json::from_str::<Value>(&out.rendered).is_err(),
            "zsh output must not be a JSON envelope"
        );
    }

    #[tokio::test]
    async fn completion_print_fish_is_raw_script() {
        let cli = demo_cli();
        let out = cli.run(["demo", "completion", "fish"]).await;
        assert_eq!(out.exit_code, 0, "fish: {}", out.rendered);
        assert!(!out.rendered.is_empty(), "fish script should be non-empty");
        assert!(
            out.rendered.contains("demo"),
            "fish script should mention bin name; got: {}",
            out.rendered
        );
        assert!(
            serde_json::from_str::<Value>(&out.rendered).is_err(),
            "fish output must not be a JSON envelope"
        );
    }

    #[tokio::test]
    async fn completion_print_powershell_is_raw_script() {
        let cli = demo_cli();
        let out = cli.run(["demo", "completion", "powershell"]).await;
        assert_eq!(out.exit_code, 0, "powershell: {}", out.rendered);
        assert!(
            !out.rendered.is_empty(),
            "powershell script should be non-empty"
        );
        assert!(
            out.rendered.contains("demo"),
            "powershell script should mention bin name; got: {}",
            out.rendered
        );
        assert!(
            serde_json::from_str::<Value>(&out.rendered).is_err(),
            "powershell output must not be a JSON envelope"
        );
    }

    #[tokio::test]
    async fn completion_print_elvish_is_raw_script() {
        let cli = demo_cli();
        let out = cli.run(["demo", "completion", "elvish"]).await;
        assert_eq!(out.exit_code, 0, "elvish: {}", out.rendered);
        assert!(
            !out.rendered.is_empty(),
            "elvish script should be non-empty"
        );
        assert!(
            out.rendered.contains("demo"),
            "elvish script should mention bin name; got: {}",
            out.rendered
        );
        assert!(
            serde_json::from_str::<Value>(&out.rendered).is_err(),
            "elvish output must not be a JSON envelope"
        );
    }

    // =========================================================================
    // (b) Auto-detect: set $SHELL, call `completion` with no shell arg.
    // =========================================================================

    #[tokio::test]
    async fn completion_autodetect_picks_bash_from_shell_env() {
        let cli = demo_cli();

        let _lock = env_lock();
        let _shell = EnvVarGuard::set("SHELL", "/usr/bin/bash");

        let out = cli.run(["demo", "completion"]).await;

        assert_eq!(out.exit_code, 0, "autodetect bash: {}", out.rendered);
        assert!(
            out.rendered.contains("demo"),
            "auto-detected bash script should mention bin name; got: {}",
            out.rendered
        );
        assert!(
            serde_json::from_str::<Value>(&out.rendered).is_err(),
            "auto-detected output must not be a JSON envelope"
        );
    }

    #[tokio::test]
    async fn completion_autodetect_picks_zsh_from_shell_env() {
        let cli = demo_cli();

        let _lock = env_lock();
        let _shell = EnvVarGuard::set("SHELL", "/bin/zsh");

        let out = cli.run(["demo", "completion"]).await;

        assert_eq!(out.exit_code, 0, "autodetect zsh: {}", out.rendered);
        assert!(
            out.rendered.contains("demo"),
            "auto-detected zsh script should mention bin name; got: {}",
            out.rendered
        );
        assert!(
            serde_json::from_str::<Value>(&out.rendered).is_err(),
            "auto-detected zsh output must not be a JSON envelope"
        );
    }

    // =========================================================================
    // (c) & (d) Install + idempotency — all env-mutating install tests run in a
    //     single serialized test function to avoid races between them.
    // =========================================================================

    #[allow(unsafe_code)]
    #[tokio::test]
    async fn completion_install_bash_writes_files_and_is_idempotent() {
        let cli = demo_cli();

        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();

        let _lock = env_lock();
        let _home = EnvVarGuard::set("HOME", home.to_str().unwrap());
        let _data = EnvVarGuard::set("XDG_DATA_HOME", data_dir.to_str().unwrap());
        let _config = EnvVarGuard::set("XDG_CONFIG_HOME", config_dir.to_str().unwrap());

        // First install.
        let out = cli.run(["demo", "completion", "--install", "bash"]).await;

        assert_eq!(out.exit_code, 0, "install bash first run: {}", out.rendered);

        // The completion script must land under the tempdir data dir.
        let script = data_dir.join("bash-completion/completions/demo");
        assert!(
            script.exists(),
            "bash completion script should exist at {}",
            script.display()
        );
        assert!(
            script.starts_with(tmp.path()),
            "script path must be under tempdir, not real HOME"
        );

        // The .bashrc managed block must exist.
        let bashrc = home.join(".bashrc");
        let content1 = std::fs::read_to_string(&bashrc).unwrap();
        assert!(
            content1.contains("# >>> demo completion (managed) >>>"),
            ".bashrc must contain opening managed block marker; got:\n{content1}"
        );
        assert!(
            content1.contains("# <<< demo completion (managed) <<<"),
            ".bashrc must contain closing managed block marker; got:\n{content1}"
        );
        assert_eq!(
            content1
                .matches("# >>> demo completion (managed) >>>")
                .count(),
            1,
            "first install: exactly one managed block"
        );

        // Second install: idempotent.
        let out2 = cli.run(["demo", "completion", "--install", "bash"]).await;
        assert_eq!(
            out2.exit_code, 0,
            "install bash second run: {}",
            out2.rendered
        );

        let content2 = std::fs::read_to_string(&bashrc).unwrap();
        assert_eq!(
            content2
                .matches("# >>> demo completion (managed) >>>")
                .count(),
            1,
            "re-install must not duplicate the managed block"
        );
    }

    #[tokio::test]
    async fn completion_install_fish_writes_script_under_config_home() {
        let cli = demo_cli();

        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let data_dir = tmp.path().join("data");
        let config_dir = tmp.path().join("config");

        let _lock = env_lock();
        let _home = EnvVarGuard::set("HOME", home.to_str().unwrap());
        let _data = EnvVarGuard::set("XDG_DATA_HOME", data_dir.to_str().unwrap());
        let _config = EnvVarGuard::set("XDG_CONFIG_HOME", config_dir.to_str().unwrap());

        let out = cli.run(["demo", "completion", "--install", "fish"]).await;
        assert_eq!(out.exit_code, 0, "install fish: {}", out.rendered);

        let fish_script = config_dir.join("fish/completions/demo.fish");
        assert!(
            fish_script.exists(),
            "fish completion script should exist at {}",
            fish_script.display()
        );
        assert!(
            fish_script.starts_with(tmp.path()),
            "fish script must be under tempdir, not real HOME"
        );
    }

    // =========================================================================
    // (e) Unknown shell → non-zero exit, no panic.
    // =========================================================================

    #[tokio::test]
    async fn completion_unknown_shell_exits_nonzero_no_panic() {
        let cli = demo_cli();
        let out = cli.run(["demo", "completion", "notashell"]).await;
        assert_ne!(out.exit_code, 0, "unknown shell must exit non-zero");
        assert!(
            out.rendered.contains("notashell") || out.rendered.contains("unsupported"),
            "error should mention the bad shell name; got: {}",
            out.rendered
        );
        // Must not be empty (error was surfaced, not silently swallowed).
        assert!(!out.rendered.is_empty(), "error output must not be empty");
    }

    #[tokio::test]
    async fn completion_install_unknown_shell_exits_nonzero_no_panic() {
        let cli = demo_cli();
        let out = cli
            .run(["demo", "completion", "--install", "notashell"])
            .await;
        assert_ne!(out.exit_code, 0, "install unknown shell must exit non-zero");
        assert!(!out.rendered.is_empty(), "error output must not be empty");
    }
}
