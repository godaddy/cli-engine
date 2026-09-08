use std::io::IsTerminal;

mod introspect;
mod register;
mod resolve;

pub use introspect::{debug_component_enabled, derive_bool_flags, derive_value_flags};
pub(crate) use register::{apply_pagination_args, compat_bool_value_parser};
pub use register::{register_global_flags, register_reason_flag};
pub use resolve::{
    app_id_env_prefix, default_output_format, extract_command_path, extract_output_format,
    global_flags_from_matches, has_true_schema_flag, min_stage_env_var, output_env_var,
    resolve_default_output_format,
};

/// Returns `true` when the process appears to be running interactively:
/// stdin and stderr are both TTYs.
///
/// Checking stdin ensures that piped input (`echo "" | gddy ...`) is detected
/// as non-interactive. Checking stderr ensures prompts can be displayed (since
/// `inquire` renders to stderr). Stdout is intentionally not checked — a user
/// piping output (`gddy ... | jq`) still has an interactive terminal for
/// prompts.
///
/// Used as the default for `GlobalFlags::interactive` when the user does not
/// pass `--interactive` or `--non-interactive` explicitly.
#[must_use]
pub fn detect_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Interactivity mode for a CLI invocation.
///
/// Commands and middleware can inspect this to decide whether to prompt for
/// missing inputs, display progress spinners, or fall back to error messages
/// suitable for scripts and CI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractivityMode {
    /// The user explicitly requested interactive prompts (`--interactive`), or
    /// the process is running in a TTY without CI indicators.
    Interactive,
    /// The user explicitly disabled prompts (`--non-interactive`), or the
    /// process is running in a non-TTY / CI context.
    NonInteractive,
}

impl InteractivityMode {
    /// Returns `true` when prompts and interactive flows are appropriate.
    #[must_use]
    pub fn is_interactive(self) -> bool {
        self == Self::Interactive
    }
}

impl From<bool> for InteractivityMode {
    fn from(interactive: bool) -> Self {
        if interactive {
            Self::Interactive
        } else {
            Self::NonInteractive
        }
    }
}

/// Parsed framework-global flags.
///
/// Applications can add their own global flags, but these are the built-in
/// controls understood by middleware and the output pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalFlags {
    /// Output format: `json`, `human`, or `toon`.
    pub output_format: String,
    /// Metadata verbosity selector.
    pub verbose: String,
    /// Whether mutating commands should short-circuit.
    pub dry_run: bool,
    /// Field projection.
    pub fields: String,
    /// Whether `fields` came from an explicit `--fields` flag on the command
    /// line, rather than clap filling in a command's `default_fields` (a
    /// command with `default_fields` set registers it as that flag's native
    /// default, so `fields` is non-empty even when the user never typed
    /// `--fields` — this is the only reliable way to tell the two apart).
    pub fields_explicit: bool,
    /// JMESPath per-item filter.
    pub filter: String,
    /// JMESPath whole-result expression.
    pub expr: String,
    /// Whether schema rendering was requested.
    pub schema: bool,
    /// User-provided command reason.
    pub reason: String,
    /// Raw timeout string.
    pub timeout: String,
    /// Debug selector.
    pub debug: String,
    /// Credential storage override from `--credential-store`, if supplied.
    pub credential_store: Option<crate::config::CredentialStore>,
    /// Interactivity mode: `true` enables prompts for missing inputs,
    /// `false` disables them. Auto-detected from TTY when neither flag is given.
    pub interactive: bool,
}

impl Default for GlobalFlags {
    fn default() -> Self {
        Self {
            output_format: "json".to_owned(),
            verbose: String::new(),
            dry_run: false,
            fields: String::new(),
            fields_explicit: false,
            filter: String::new(),
            expr: String::new(),
            schema: false,
            reason: String::new(),
            timeout: "0s".to_owned(),
            debug: String::new(),
            credential_store: None,
            interactive: detect_interactive(),
        }
    }
}

/// Explicit `--help` display-order values for the engine's own global flags,
/// numbered in the order they're registered below — which is meant to read
/// as their relative importance, most-used first.
///
/// Without this, every global flag would collide with command-specific
/// ones: clap auto-assigns each unset `display_order` as "the Nth argument
/// added to this `Command`," starting the count over at 0 on every
/// `Command` it's called on — the root (where these are declared) and each
/// subcommand alike. A subcommand's own `CommandSpec::with_arg` args get
/// low counter values (0, 1, 2, ... in declaration order) from their own
/// `Command`; a global flag propagated onto that subcommand keeps the low
/// counter value it got on the *root*. Mix the two and `--help` interleaves
/// them instead of showing command-specific flags first, as a block, in the
/// order they were declared. Parking every global flag comfortably above
/// any realistic per-command arg count keeps that from happening.
///
/// `FIELDS`, `FILTER`, and `EXPR` are `pub(crate)` because `cli.rs`
/// re-registers those three per-command (see `apply_fields_arg` and
/// `apply_filter_and_expr_examples`) with contextual help text; they must
/// reuse these same values or the override would drift out of position.
///
/// `LIMIT` and `OFFSET` are never registered by [`register_global_flags`]
/// itself — unlike every other value here, `--limit`/`--offset` are not
/// framework-global at all; `cli.rs` registers them directly on a single
/// command's own `Command` (see `apply_pagination_args`), and only for a
/// command that opted in via `CommandSpec::with_pagination`. These two
/// constants exist purely so that per-command registration still parks the
/// flags in the same relative `--help` position other engine flags occupy.
///
/// `REASON` and `ENV` cover the two global flags `Cli::new` registers
/// directly (conditionally, outside `register_global_flags`) rather than
/// this module's own function — `--reason` when an authorizer/auditor/
/// activity emitter is configured, `--env` when `CliConfig.environments` is
/// set. Both are just as subject to the collision this module exists to
/// prevent, so both need an explicit value here too.
pub(crate) mod global_flag_order {
    pub(crate) const HELP: usize = 1000;
    pub(crate) const OUTPUT: usize = 1001;
    pub(crate) const VERBOSE: usize = 1002;
    pub(crate) const DRY_RUN: usize = 1003;
    pub(crate) const FIELDS: usize = 1004;
    pub(crate) const FILTER: usize = 1005;
    pub(crate) const EXPR: usize = 1006;
    pub(crate) const LIMIT: usize = 1007;
    pub(crate) const OFFSET: usize = 1008;
    pub(crate) const SCHEMA: usize = 1009;
    pub(crate) const TIMEOUT: usize = 1010;
    pub(crate) const DEBUG: usize = 1011;
    pub(crate) const CREDENTIAL_STORE: usize = 1012;
    pub(crate) const JSON: usize = 1013;
    pub(crate) const TOON: usize = 1014;
    pub(crate) const HUMAN: usize = 1015;
    pub(crate) const INTERACTIVE: usize = 1016;
    pub(crate) const REASON: usize = 1017;
    pub(crate) const ENV: usize = 1018;
}

#[cfg(test)]
mod tests {
    use clap::Command;

    use super::{
        debug_component_enabled, min_stage_env_var, output_env_var, register_global_flags,
        resolve_default_output_format,
    };

    #[test]
    fn debug_component_matcher_handles_wildcards_and_negation() {
        // Empty pattern enables nothing.
        assert!(!debug_component_enabled("", "transport"));
        // Wildcard enables everything.
        assert!(debug_component_enabled("*", "transport"));
        assert!(debug_component_enabled("*", "auth"));
        // Bare name enables only that component.
        assert!(debug_component_enabled("transport", "transport"));
        assert!(!debug_component_enabled("transport", "auth"));
        // Negation after a wildcard removes one component but keeps the rest.
        assert!(!debug_component_enabled("*,-transport", "transport"));
        assert!(debug_component_enabled("*,-auth", "transport"));
        // `-*` disables everything; later tokens still win.
        assert!(!debug_component_enabled("*,-*", "transport"));
        assert!(debug_component_enabled("-*,transport", "transport"));
        // Whitespace and case are ignored.
        assert!(debug_component_enabled(" Transport , -auth ", "transport"));
        // An empty component fails closed, even against a wildcard.
        assert!(!debug_component_enabled("*", ""));
        assert!(!debug_component_enabled("*", "   "));
    }

    #[test]
    fn default_output_format_follows_env_override_then_tty() {
        // TTY policy when no env or config override.
        assert_eq!(resolve_default_output_format(None, None, true), "human");
        assert_eq!(resolve_default_output_format(None, None, false), "json");
        // A valid env override wins over the TTY policy in both directions.
        assert_eq!(
            resolve_default_output_format(Some("json"), None, true),
            "json"
        );
        assert_eq!(
            resolve_default_output_format(Some("human"), None, false),
            "human"
        );
        // Env override is case-insensitive (env vars are commonly upper-cased).
        assert_eq!(
            resolve_default_output_format(Some("JSON"), None, true),
            "json"
        );
        assert_eq!(
            resolve_default_output_format(Some(" Human "), None, false),
            "human"
        );
        // Blank or unrecognized env overrides are ignored (fall back to TTY).
        assert_eq!(
            resolve_default_output_format(Some("   "), None, false),
            "json"
        );
        assert_eq!(resolve_default_output_format(Some(""), None, true), "human");
        assert_eq!(
            resolve_default_output_format(Some("yaml"), None, false),
            "json"
        );
        assert_eq!(
            resolve_default_output_format(Some("yaml"), None, true),
            "human"
        );
    }

    #[test]
    fn default_output_format_config_override_wins_over_tty_but_not_env() {
        // Config override wins over the TTY policy when there's no env override.
        assert_eq!(
            resolve_default_output_format(None, Some("json"), true),
            "json"
        );
        assert_eq!(
            resolve_default_output_format(None, Some("human"), false),
            "human"
        );
        // Env override still wins over a config override.
        assert_eq!(
            resolve_default_output_format(Some("human"), Some("json"), false),
            "human"
        );
        // Blank or unrecognized config overrides are ignored (fall back to TTY).
        assert_eq!(
            resolve_default_output_format(None, Some("yaml"), true),
            "human"
        );
        assert_eq!(
            resolve_default_output_format(None, Some("yaml"), false),
            "json"
        );
    }

    #[test]
    fn output_env_var_is_derived_from_app_id() {
        assert_eq!(output_env_var("godaddy"), "GODADDY_OUTPUT");
        assert_eq!(output_env_var("gdx"), "GDX_OUTPUT");
        assert_eq!(output_env_var("my-cli"), "MY_CLI_OUTPUT");
    }

    #[test]
    fn min_stage_env_var_is_derived_from_app_id() {
        assert_eq!(min_stage_env_var("godaddy"), "GODADDY_MIN_STAGE");
        assert_eq!(min_stage_env_var("gdx"), "GDX_MIN_STAGE");
        assert_eq!(min_stage_env_var("my-cli"), "MY_CLI_MIN_STAGE");
    }

    #[test]
    fn short_and_long_help_flags_render_identical_output() {
        let build = || {
            register_global_flags(Command::new("testcli"))
                .subcommand(Command::new("sub").about("A subcommand"))
        };
        let help_text = |args: &[&str]| {
            build()
                .try_get_matches_from(args)
                .expect_err("help action short-circuits parsing")
                .to_string()
        };

        assert_eq!(
            help_text(&["testcli", "-h"]),
            help_text(&["testcli", "--help"])
        );
        assert_eq!(
            help_text(&["testcli", "sub", "-h"]),
            help_text(&["testcli", "sub", "--help"])
        );
    }

    #[test]
    fn interactivity_mode_from_bool() {
        use super::InteractivityMode;
        assert_eq!(
            InteractivityMode::from(true),
            InteractivityMode::Interactive
        );
        assert_eq!(
            InteractivityMode::from(false),
            InteractivityMode::NonInteractive
        );
        assert!(InteractivityMode::Interactive.is_interactive());
        assert!(!InteractivityMode::NonInteractive.is_interactive());
    }

    #[test]
    fn interactive_flag_parsing_explicit_interactive() {
        use super::global_flags_from_matches;
        let cmd = register_global_flags(Command::new("test"));
        let matches = cmd
            .try_get_matches_from(["test", "--interactive"])
            .expect("should parse");
        // --interactive works even when auto_interactive is false
        let flags = global_flags_from_matches(&matches, "json", false);
        assert!(flags.interactive);
    }

    #[test]
    fn interactive_flag_parsing_explicit_non_interactive() {
        use super::global_flags_from_matches;
        let cmd = register_global_flags(Command::new("test"));
        let matches = cmd
            .try_get_matches_from(["test", "--non-interactive"])
            .expect("should parse");
        // --non-interactive wins even when auto_interactive is true
        let flags = global_flags_from_matches(&matches, "json", true);
        assert!(!flags.interactive);
    }

    #[test]
    fn interactive_defaults_off_without_auto_interactive() {
        use super::global_flags_from_matches;
        let cmd = register_global_flags(Command::new("test"));
        let matches = cmd.try_get_matches_from(["test"]).expect("should parse");
        // No explicit flag + auto_interactive=false → not interactive
        let flags = global_flags_from_matches(&matches, "json", false);
        assert!(!flags.interactive);
    }

    #[test]
    fn interactive_flag_conflicts() {
        let cmd = register_global_flags(Command::new("test"));
        let result = cmd.try_get_matches_from(["test", "--interactive", "--non-interactive"]);
        assert!(result.is_err());
    }

    #[test]
    fn detect_interactive_is_consistent_with_tty_state() {
        // detect_interactive checks stdin + stderr TTY state.
        // In CI (no real TTY), both are typically non-terminals → false.
        // Locally in a real terminal, both are terminals → true.
        // Either way, it should not panic and should be consistent.
        let result = super::detect_interactive();
        let stdin_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
        let stderr_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
        assert_eq!(result, stdin_tty && stderr_tty);
    }
}
