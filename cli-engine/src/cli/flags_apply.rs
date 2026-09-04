//! Applying parsed global/pagination flags to a per-run [`Middleware`],
//! building the "next page" command replay string, transport debug-logger
//! wiring, and small argv/env parsing helpers used before clap ever runs.

use std::time::Duration;

use clap::{Arg, ArgMatches};

use crate::{
    CliCoreError, CommandSpec, Middleware, Result,
    feature_flags::Stage,
    flags::{GlobalFlags, min_stage_env_var},
};

pub(super) fn apply_global_flags(
    middleware: &mut Middleware,
    flags: &GlobalFlags,
    timeout: Option<Duration>,
) {
    middleware.output_format = flags.output_format.clone();
    middleware.verbose = flags.verbose.clone();
    middleware.dry_run = flags.dry_run;
    middleware.fields = flags.fields.clone();
    middleware.fields_explicit = flags.fields_explicit;
    middleware.filter = flags.filter.clone();
    middleware.expr = flags.expr.clone();
    middleware.reason = flags.reason.clone();
    middleware.schema = flags.schema;
    middleware.timeout = timeout;
    middleware.debug = flags.debug.clone();
    middleware.interactive = flags.interactive;
}

/// Sets `middleware.limit`/`middleware.offset` from a paginating command's own
/// `--limit`/`--offset`
pub(super) fn apply_pagination_flags(
    middleware: &mut Middleware,
    spec: &CommandSpec,
    leaf: &ArgMatches,
) {
    let Some(pagination) = spec.pagination else {
        return;
    };
    middleware.limit = leaf
        .get_one::<i64>("limit")
        .copied()
        .unwrap_or(pagination.default_limit);
    middleware.offset = leaf.get_one::<i64>("offset").copied().unwrap_or(0);
}

/// Replays a paginating command's own explicit args, plus the global
/// `--filter`/`--expr`/`--fields` flags, as `--flag value` text, prefixed
/// with the CLI's binary name — the base a "view the next page"
/// [`crate::NextAction`] is built from once the response's
/// [`crate::PaginationMeta`] is known. Leading with the binary name keeps the
/// suggested command copy-pastable rather than a fragment starting at the
/// noun/verb path.
///
/// `--filter`/`--expr`/`--fields` sit in the same output pipeline as
/// pagination itself (filter -> paginate -> expr -> fields) and change what
/// data comes back, so dropping them would make the suggested next-page
/// command return different results than the command the user actually ran.
/// Other global flags (`--output`, `--verbose`, `--env`, ...) don't affect
/// *which* data is returned, so they're intentionally left out — the caller
/// is already running under them.
///
/// Best-effort, not a fully general clap-args reconstruction: it uses each
/// arg's real `get_long()`/`get_short()` name (never the value-map key,
/// which for derive-based args can differ from the flag — e.g. id
/// `page_size` vs flag `--page-size`), replays a multi-value arg as one
/// flag occurrence per value (round-trips correctly whether the arg is a
/// plain repeatable `ArgAction::Append` or also sets a `value_delimiter`),
/// and quotes/escapes values containing whitespace or shell metacharacters
/// (see `quote_pagination_value`). Deliberately omits `--limit`/`--offset` —
/// those are added by the caller once it knows the
/// next page's offset.
pub(super) fn pagination_command_base(
    binary_name: &str,
    command_path: &str,
    spec: &CommandSpec,
    user_args: &crate::middleware::ValueMap,
    flags: &GlobalFlags,
) -> String {
    let mut parts = vec![
        quote_pagination_value(binary_name),
        command_path.replace(':', " "),
    ];
    for arg in &spec.args {
        let id = arg.get_id().as_str();
        if let Some(value) = user_args.get(id) {
            push_pagination_arg(&mut parts, arg, value);
        }
    }
    for (flag, value) in [
        ("--filter", &flags.filter),
        ("--expr", &flags.expr),
        ("--fields", &flags.fields),
    ] {
        if !value.is_empty() {
            parts.push(flag.to_owned());
            parts.push(quote_pagination_value(value));
        }
    }
    parts.join(" ")
}

fn push_pagination_arg(parts: &mut Vec<String>, arg: &Arg, value: &serde_json::Value) {
    let flag = arg
        .get_long()
        .map(|long| format!("--{long}"))
        .or_else(|| arg.get_short().map(|short| format!("-{short}")));
    match value {
        serde_json::Value::Bool(enabled) => {
            if matches!(
                arg.get_action(),
                clap::ArgAction::SetTrue | clap::ArgAction::SetFalse
            ) {
                // A switch-style flag's presence in `user_args` already means
                // the user typed exactly this flag — `SetTrue` implies `true`,
                // `SetFalse` implies `false` (e.g. a `--no-foo`-style arg) —
                // and neither accepts an explicit `=value` token, so replay
                // the bare flag rather than appending one.
                if let Some(flag) = flag {
                    parts.push(flag);
                }
            } else {
                // A custom bool-valued arg (`ArgAction::Set` with a bool
                // value parser) takes an explicit token, so replay it like
                // any other scalar.
                push_flagged_value(parts, flag, &enabled.to_string());
            }
        }
        serde_json::Value::Array(items) => {
            // Repeat the flag once per value rather than joining into one
            // comma-separated token: clap collects a repeatable flag
            // (`ArgAction::Append`, the common way a command declares a
            // multi-value arg) the same way whether or not it also sets
            // `value_delimiter(',')`, so `--scope a --scope b` round-trips
            // correctly either way. A single `--scope a,b` only works when
            // a delimiter was configured — for a plain `Append` arg it's
            // parsed as one literal value, changing the replay's meaning.
            for item in items {
                push_flagged_value(parts, flag.clone(), &pagination_arg_display(item));
            }
        }
        serde_json::Value::Null => {}
        other => push_flagged_value(parts, flag, &pagination_arg_display(other)),
    }
}

fn push_flagged_value(parts: &mut Vec<String>, flag: Option<String>, value: &str) {
    if let Some(flag) = flag {
        parts.push(flag);
    }
    parts.push(quote_pagination_value(value));
}

fn pagination_arg_display(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Quotes a value for the suggested next-page command, if it contains
/// anything beyond a small safe-unquoted allowlist. Whitespace and shell
/// metacharacters (`|`, `&`, `;`, `<`, `>`, ...) all fall outside that
/// allowlist and so trigger quoting; once quoted, `\`, `"`, `$`, and `` ` ``
/// are backslash-escaped (backslash first, so escaping the others doesn't
/// re-escape the backslashes it just inserted) so the value can't break out
/// of the double quotes or trigger POSIX-shell expansion (`$VAR`, `$(...)`,
/// backticks) if the suggestion is copy-pasted into a shell.
fn quote_pagination_value(value: &str) -> String {
    let safe_unquoted =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@');
    if value.is_empty() || !value.chars().all(safe_unquoted) {
        let escaped = value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
            .replace('`', "\\`");
        format!("\"{escaped}\"")
    } else {
        value.to_owned()
    }
}

/// Builds the transport debug logger implied by a parsed `--debug` pattern,
/// without publishing it anywhere.
///
/// Pure so tests can assert on the decision (`--debug` pattern -> enabled or
/// not) without touching the process-wide default logger, which every
/// [`Cli::run`](super::Cli::run) call republishes — including the many unrelated tests that
/// exercise `cli.run(...)` with no `--debug` flag and would otherwise race
/// with an assertion on the shared global.
fn debug_transport_logger_for(
    debug: &str,
    extra_redacted: &[String],
) -> std::sync::Arc<dyn crate::transport::TransportLogger> {
    if crate::debug_component_enabled(debug, "transport") {
        std::sync::Arc::new(
            crate::transport::StderrTransportLogger::new()
                .with_redacted_headers(extra_redacted.iter().cloned()),
        )
    } else {
        std::sync::Arc::new(crate::transport::NoopTransportLogger)
    }
}

/// Installs (or clears) the process-wide transport debug logger from the parsed
/// `--debug` pattern.
///
/// When `--debug` selects the `transport` component the engine publishes a
/// [`StderrTransportLogger`](crate::transport::StderrTransportLogger) — extended
/// with any [`CliConfig::with_redacted_debug_headers`](super::CliConfig::with_redacted_debug_headers) entries — which every
/// [`HttpClient`](crate::transport::HttpClient) built afterward picks up
/// automatically, with no per-command wiring. The logger is reset to a noop when
/// `transport` is not selected so the explicit setting always reflects the
/// current invocation rather than a stale process-global from an earlier one.
pub(super) fn install_debug_transport_logger(debug: &str, extra_redacted: &[String]) {
    crate::transport::set_default_transport_logger(debug_transport_logger_for(
        debug,
        extra_redacted,
    ));
}

pub(super) fn parse_command_timeout(raw: &str) -> Result<Option<Duration>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Some(Duration::from_secs(60)));
    }
    let Some(seconds) = parse_duration_seconds(raw) else {
        return Err(CliCoreError::message(format!(
            "invalid timeout {raw:?}: expected duration like 60s, 5m, or 0s"
        )));
    };
    if seconds <= 0.0 {
        Ok(None)
    } else {
        Ok(Some(Duration::from_secs_f64(seconds)))
    }
}

fn parse_duration_seconds(raw: &str) -> Option<f64> {
    for (suffix, seconds) in [
        ("ns", 0.000_000_001_f64),
        ("us", 0.000_001_f64),
        ("µs", 0.000_001_f64),
        ("ms", 0.001_f64),
        ("s", 1.0_f64),
        ("m", 60.0_f64),
        ("h", 3600.0_f64),
    ] {
        if let Some(number) = raw.strip_suffix(suffix) {
            let value = number.parse::<f64>().ok()?;
            if !value.is_finite() {
                return None;
            }
            return Some(value * seconds);
        }
    }
    None
}

/// Reads the global `${APP_ID}_MIN_STAGE` override (see [`min_stage_env_var`]).
///
/// Best-effort, like [`crate::config::ConfigFile::load`]'s handling of a
/// malformed config file: returns `None` when the var is unset, and also
/// `None` (after logging a warning) when it is set but fails to parse as a
/// [`Stage`], so a typo'd value cannot take the CLI down.
pub(super) fn global_min_stage_override(app_id: &str) -> Option<Stage> {
    let var = min_stage_env_var(app_id);
    let value = std::env::var(&var).ok()?;
    value.parse::<Stage>().map_or_else(
        |err| {
            tracing::warn!(var = %var, value = %value, error = %err, "ignoring invalid min-stage override");
            None
        },
        Some,
    )
}

/// Pure scan over an arg iterator for the last `--env <value>`/`--env=<value>`
/// occurrence — used only to seed [`Cli::new`](super::Cli::new)'s `flag_policy` (and therefore
/// which flagged commands get pruned) before the command tree is built, since
/// that decision can't be revisited once real argv is parsed. The real,
/// per-invocation `--env` value used for dispatch still comes from
/// `apply_env_flag`'s clap-based parse, unchanged; this scan never replaces
/// it, only decides tree shape earlier than clap otherwise could
/// (clap's own [`clap::Command::ignore_errors`] does not help here — it
/// still requires the rest of the argv to parse against a *known* subcommand
/// structure, and at prescan time no domain modules are registered yet, so a
/// real command path makes it bail on capturing global flags too).
///
/// Scans the *entire* argv and keeps the *last* non-empty `--env`/`--env=`
/// value, rather than stopping at the first match — a global `--env` and a
/// command-local one sharing the same arg id can both appear in one
/// invocation, and whichever clap resolves as the effective value
/// (empirically, the last one) is the one this scan must agree with. An
/// empty value (`--env=` with nothing after the `=`, or `--env` immediately
/// followed by another flag with nothing captured) is ignored rather than
/// becoming a literal empty-string candidate.
pub(super) fn prescan_env_flag(mut args: impl Iterator<Item = String>) -> Option<String> {
    let mut result = None;
    while let Some(arg) = args.next() {
        // clap's end-of-options sentinel: everything after a bare `--` is a
        // positional argument, never a flag, no matter what it looks like.
        // This scan must agree, or `app cmd -- --env dev` would be
        // misread as a real `--env` override.
        if arg == "--" {
            break;
        }
        let value = if let Some(v) = arg.strip_prefix("--env=") {
            Some(v.to_owned())
        } else if arg == "--env" {
            // A space-separated value that itself looks like another flag
            // (starts with `-`) is not a value at all — clap rejects this
            // outright ("a value is required for '--env <ENV>' but none was
            // supplied"), so this scan must not treat it as one either. An
            // explicit `--env=-foo` is unambiguous and still accepted, same
            // as clap's own disambiguation rule.
            args.next().filter(|v| !v.starts_with('-'))
        } else {
            None
        };
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            result = Some(v);
        }
    }
    result
}

#[cfg(test)]
mod user_agent_tests {
    use super::*;
    use crate::cli::{BuildInfo, Cli, CliConfig};

    #[test]
    fn user_agent_string_derives_name_and_version_by_default() {
        let config =
            CliConfig::new("gdx", "GoDaddy CLI", "gdx").with_build(BuildInfo::new("1.2.3"));
        assert_eq!(config.user_agent_string(), "gdx/1.2.3");
    }

    #[test]
    fn user_agent_string_prefers_explicit_override() {
        let config = CliConfig::new("gdx", "GoDaddy CLI", "gdx")
            .with_build(BuildInfo::new("1.2.3"))
            .with_user_agent("gdx-cli/9.9 (custom)");
        assert_eq!(config.user_agent_string(), "gdx-cli/9.9 (custom)");
    }

    #[test]
    fn user_agent_string_omits_version_when_absent() {
        let config = CliConfig::new("gdx", "GoDaddy CLI", "gdx");
        assert_eq!(config.user_agent_string(), "gdx");
    }

    #[test]
    fn install_default_user_agent_publishes_config_value() {
        let _guard = crate::transport::client::UA_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _restore = crate::transport::client::RestoreDefaultUserAgent;
        crate::transport::set_default_user_agent("cli/dev");
        let cli = Cli::new(
            CliConfig::new("uatest", "UA test", "uatest").with_build(BuildInfo::new("4.5.6")),
        );
        cli.install_default_user_agent();
        assert_eq!(
            crate::transport::client::default_user_agent(),
            "uatest/4.5.6"
        );
    }

    #[test]
    fn install_debug_transport_logger_tracks_the_debug_pattern() {
        // Asserts on `debug_transport_logger_for`'s decision directly rather
        // than publishing to and reading back the process-wide default
        // logger, which `Cli::run` republishes on every call — including the
        // many unrelated tests that call `cli.run(...)` with no `--debug`
        // flag and would otherwise race with this assertion.

        // `transport` selected -> an active (enabled) logger is built.
        assert!(debug_transport_logger_for("transport", &[]).enabled());

        // Wildcard with transport excluded -> a disabled (noop) logger.
        assert!(!debug_transport_logger_for("*,-transport", &[]).enabled());

        // Empty pattern -> disabled (noop).
        assert!(!debug_transport_logger_for("", &[]).enabled());
    }
}

#[cfg(test)]
mod env_config_tests {
    use std::sync::Arc;

    use crate::cli::{Cli, CliConfig};

    #[test]
    fn with_environments_stores_shared_arc_with_consumer_app_id() {
        // The consumer sets app_id on the Environments before sharing the Arc;
        // CliConfig stores it as-is, so the file path resolves only because the
        // consumer stamped the matching app_id (not because the engine did).
        let cfg = CliConfig::new("gddy", "GoDaddy CLI", "gddy").with_environments(Arc::new(
            crate::environments::Environments::new("prod")
                .with_app_id("gddy")
                .with_config_file(true),
        ));
        let envs = cfg.environments.as_ref().expect("environments set");
        assert!(envs.config_file_path().is_some());
    }

    #[tokio::test]
    async fn env_flag_overrides_default_and_reaches_middleware_env() {
        use crate::{CommandResult, CommandSpec, RuntimeCommandSpec};
        use serde_json::json;
        let mut cli = Cli::new(
            CliConfig::new("envtest", "Env test", "envtest")
                .with_environments(Arc::new(
                    crate::environments::Environments::new("prod")
                        .with_environment("prod", crate::environments::EnvTable::new())
                        .with_environment("ote", crate::environments::EnvTable::new()),
                ))
                .with_startup_args(Vec::<&str>::new()),
        );
        cli.add_command(RuntimeCommandSpec::new_with_context(
            CommandSpec::new("whichenv", "echo env").no_auth(true),
            async |ctx| {
                Ok(CommandResult::new(
                    json!({ "env": ctx.environment()?.name().to_owned() }),
                ))
            },
        ));
        let out = cli
            .run(["envtest", "whichenv", "--env", "ote", "--output", "json"])
            .await;
        assert_eq!(out.exit_code, 0, "rendered: {}", out.rendered);
        assert!(out.rendered.contains("\"env\""));
        assert!(out.rendered.contains("ote"));
    }

    #[tokio::test]
    async fn unknown_env_flag_produces_error_envelope() {
        let cli = Cli::new(
            CliConfig::new("envtest2", "Env test", "envtest2")
                .with_environments(Arc::new(
                    crate::environments::Environments::new("prod")
                        .with_environment("prod", crate::environments::EnvTable::new()),
                ))
                .with_startup_args(Vec::<&str>::new()),
        );
        let out = cli.run(["envtest2", "tree", "--env", "nope"]).await;
        assert_ne!(out.exit_code, 0);
        assert!(out.rendered.contains("nope"));
    }
}

#[cfg(test)]
mod prescan_env_flag_tests {
    use super::*;

    fn argv(args: &[&str]) -> impl Iterator<Item = String> {
        args.iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn finds_space_separated_value() {
        assert_eq!(
            prescan_env_flag(argv(&["--dry-run", "--env", "dev", "list"])),
            Some("dev".to_owned())
        );
    }

    #[test]
    fn finds_equals_separated_value() {
        assert_eq!(
            prescan_env_flag(argv(&["--env=dev", "list"])),
            Some("dev".to_owned())
        );
    }

    #[test]
    fn is_none_without_the_flag() {
        assert_eq!(prescan_env_flag(argv(&["env", "list"])), None);
    }

    #[test]
    fn trailing_env_flag_with_no_value_is_none() {
        assert_eq!(prescan_env_flag(argv(&["--env"])), None);
    }

    #[test]
    fn keeps_the_last_of_multiple_occurrences() {
        // A global `--env` and a command-local one sharing the same arg id
        // can both appear (e.g. `app --env bar sub --env foo ...`); clap
        // resolves the *last* one as effective, so this scan must too.
        assert_eq!(
            prescan_env_flag(argv(&["--env", "bar", "sub", "cmd", "--env", "foo", "arg"])),
            Some("foo".to_owned())
        );
    }

    #[test]
    fn ignores_an_empty_equals_value() {
        assert_eq!(prescan_env_flag(argv(&["--env="])), None);
    }

    #[test]
    fn empty_occurrence_does_not_clobber_an_earlier_real_value() {
        assert_eq!(
            prescan_env_flag(argv(&["--env", "dev", "--env="])),
            Some("dev".to_owned())
        );
    }

    #[test]
    fn space_separated_value_starting_with_dash_is_not_a_value() {
        // clap rejects `--env --dry-run` outright ("a value is required for
        // '--env <ENV>' but none was supplied") rather than treating
        // `--dry-run` as the value; this scan must agree.
        assert_eq!(prescan_env_flag(argv(&["--env", "--dry-run"])), None);
    }

    #[test]
    fn equals_form_accepts_a_value_starting_with_dash() {
        // `--env=-foo` is unambiguous (unlike the space-separated form) and
        // still accepted, matching clap's own disambiguation rule.
        assert_eq!(
            prescan_env_flag(argv(&["--env=-foo"])),
            Some("-foo".to_owned())
        );
    }

    #[test]
    fn stops_at_the_end_of_options_sentinel() {
        // Everything after a bare `--` is positional to clap, never a flag —
        // `app cmd -- --env dev` must not be read as a real `--env` override.
        assert_eq!(prescan_env_flag(argv(&["cmd", "--", "--env", "dev"])), None);
    }

    #[test]
    fn a_real_flag_before_the_sentinel_is_still_found() {
        assert_eq!(
            prescan_env_flag(argv(&["--env", "dev", "--", "positional"])),
            Some("dev".to_owned())
        );
    }
}
