use std::io::IsTerminal;

use clap::{Arg, ArgAction, Command, builder::ValueParser};

use super::global_flag_order;

/// Registers framework-global flags on a `clap` command.
pub fn register_global_flags(command: Command) -> Command {
    command
        .disable_help_flag(true)
        .arg(
            // clap's default help arg shows an abbreviated summary for `-h`
            // and the full help text for `--help`. Override it so both
            // flags print the same full help everywhere; `disable_help_flag`
            // propagates to every subcommand.
            Arg::new("help")
                .short('h')
                .long("help")
                .action(ArgAction::HelpLong)
                .global(true)
                .display_order(global_flag_order::HELP)
                .help("Print help"),
        )
        .arg(
            Arg::new("output")
                .long("output")
                .short('o')
                .global(true)
                .display_order(global_flag_order::OUTPUT)
                .value_name("FORMAT")
                // This default is cosmetic, not authoritative: it's never
                // actually read as a value — `global_flags_from_matches` only
                // consults this arg when it was given on the command line,
                // falling back to `resolve_default_output_format`'s full
                // env/config/TTY precedence the rest of the time. But since
                // `--help` runs in this same process, this process's own
                // stdout TTY-ness is already known and stable for the whole
                // run, so mirroring that one signal here (skipping the
                // env-var/config-file tiers, which aren't available until a
                // command actually executes) keeps what `--help` shows honest
                // in the common case instead of a hardcoded, often-wrong
                // `[default: json]`.
                .default_value(if std::io::stdout().is_terminal() {
                    "human"
                } else {
                    "json"
                })
                // Only conflicts when *explicitly* given: clap's conflict
                // checks ignore an arg's default value, so a bare `--json`
                // with no `--output` at all is unaffected.
                .conflicts_with_all(["json", "toon", "human"])
                .help(
                    "Output format: toon|json|human (shorthand: --json, --toon, --human); \
                     defaults to human in an interactive terminal, json otherwise",
                ),
        )
        .arg(
            Arg::new("verbose")
                .long("verbose")
                .global(true)
                .num_args(0..=1)
                .default_missing_value("all")
                .value_name("FIELDS")
                .display_order(global_flag_order::VERBOSE)
                .help("Include metadata in output (all, or comma-separated: system,duration,args,env,identity,command,effective_args,timestamp)"),
        )
        .arg(
            Arg::new("dry-run")
                .long("dry-run")
                .global(true)
                .num_args(0..=1)
                .require_equals(true)
                .default_missing_value("true")
                .default_value("false")
                .value_parser(compat_bool_value_parser())
                .display_order(global_flag_order::DRY_RUN)
                .help("Preview mutations without executing"),
        )
        .arg(
            Arg::new("fields")
                .long("fields")
                .global(true)
                .value_name("FIELDS")
                .display_order(global_flag_order::FIELDS)
                .help("Comma-separated fields to include in output (use 'all' or '*' for everything)"),
        )
        .arg(
            Arg::new("filter")
                .long("filter")
                .global(true)
                .value_name("EXPR")
                .display_order(global_flag_order::FILTER)
                .help("Per-item JMESPath predicate for list data"),
        )
        .arg(
            Arg::new("expr")
                .long("expr")
                .global(true)
                .value_name("EXPR")
                .display_order(global_flag_order::EXPR)
                .help("JMESPath query applied to the whole result"),
        )
        .arg(
            Arg::new("schema")
                .long("schema")
                .global(true)
                .num_args(0..=1)
                .require_equals(true)
                .default_missing_value("true")
                .default_value("false")
                .value_parser(compat_bool_value_parser())
                .display_order(global_flag_order::SCHEMA)
                .help("Dump output field metadata instead of running the command"),
        )
        .arg(
            Arg::new("timeout")
                .long("timeout")
                .global(true)
                .allow_hyphen_values(true)
                .default_value("0s")
                .value_name("DURATION")
                .display_order(global_flag_order::TIMEOUT)
                .help("Overall command timeout (e.g. 60s, 5m); default 0s = no timeout"),
        )
        .arg(
            Arg::new("debug")
                .long("debug")
                .global(true)
                .num_args(0..=1)
                .default_missing_value("*")
                .value_name("PATTERN")
                .display_order(global_flag_order::DEBUG)
                .help("Enable debug logging (comma-separated component patterns, e.g. *, transport, *,-auth)"),
        )
        .arg(
            Arg::new("credential-store")
                .long("credential-store")
                .display_order(global_flag_order::CREDENTIAL_STORE)
                .global(true)
                .value_name("MODE")
                .value_parser(|s: &str| s.parse::<crate::config::CredentialStore>())
                .help("Credential storage: auto|keyring|file (overrides env and config)"),
        )
        .arg(
            Arg::new("interactive")
                .long("interactive")
                .short('i')
                .global(true)
                .action(ArgAction::SetTrue)
                .conflicts_with("non-interactive")
                .display_order(global_flag_order::INTERACTIVE)
                .help("Force interactive prompts for missing inputs (default when TTY is detected)"),
        )
        .arg(
            Arg::new("non-interactive")
                .long("non-interactive")
                .global(true)
                .action(ArgAction::SetTrue)
                .conflicts_with("interactive")
                .hide(true)
                .display_order(global_flag_order::INTERACTIVE)
                .help("Disable interactive prompts; fail on missing required inputs"),
        )
        .arg(
            Arg::new("json")
                .long("json")
                .global(true)
                .action(ArgAction::SetTrue)
                // Mutually exclusive with the other format selectors, so
                // e.g. `--json --human` together is a usage error rather
                // than one silently overriding the other.
                .conflicts_with_all(["toon", "human"])
                // Documented on `--output` instead of taking their own line
                // in every command's already-long options list.
                .hide(true)
                .display_order(global_flag_order::JSON)
                .help("Shorthand for --output json"),
        )
        .arg(
            Arg::new("toon")
                .long("toon")
                .global(true)
                .action(ArgAction::SetTrue)
                .conflicts_with_all(["json", "human"])
                .hide(true)
                .display_order(global_flag_order::TOON)
                .help("Shorthand for --output toon"),
        )
        .arg(
            Arg::new("human")
                .long("human")
                .global(true)
                .action(ArgAction::SetTrue)
                .conflicts_with_all(["json", "toon"])
                .hide(true)
                .display_order(global_flag_order::HUMAN)
                .help("Shorthand for --output human"),
        )
}

/// Registers the `--reason` flag on a `clap` command.
///
/// Not part of [`register_global_flags`]: `--reason` is only meaningful when an
/// app has registered an [`Authorizer`](crate::middleware::Authorizer),
/// [`Auditor`](crate::middleware::Auditor), or
/// [`ActivityEmitter`](crate::middleware::ActivityEmitter) to consume it (see
/// `Cli::new`'s conditional call to this function). Apps with none of those
/// configured never register this flag at all, rather than exposing a flag
/// that nothing reads. `Cli::new` only checks the eager `authz`/`auditor`/
/// `activity` fields on `CliConfig`; installing one of these later via
/// `init_deps` does not register `--reason`, since flag registration happens
/// before `init_deps` runs.
pub fn register_reason_flag(command: Command) -> Command {
    command.arg(
        Arg::new("reason")
            .long("reason")
            .global(true)
            .value_name("TEXT")
            .display_order(global_flag_order::REASON)
            .help("Short explanation of why this command is being run (forwarded to your authorizer, auditor, or activity emitter)"),
    )
}

/// Registers `--limit`/`--offset` directly on one command's own `clap`
/// `Command`, for a command whose [`CommandSpec`](crate::CommandSpec) opted
/// into pagination via `with_pagination`.
pub(crate) fn apply_pagination_args(
    command: Command,
    default_limit: i64,
    max_limit: i64,
) -> Command {
    command
        .arg(
            Arg::new("limit")
                .long("limit")
                .value_parser(pagination_limit_value_parser(max_limit))
                .allow_hyphen_values(true)
                .default_value(default_limit.to_string())
                .display_order(global_flag_order::LIMIT)
                .help(pagination_limit_help(default_limit, max_limit)),
        )
        .arg(
            Arg::new("offset")
                .long("offset")
                .value_parser(pagination_offset_value_parser())
                .allow_hyphen_values(true)
                .default_value("0")
                .display_order(global_flag_order::OFFSET)
                .help("Skip N items before applying limit"),
        )
}

fn pagination_limit_help(default_limit: i64, max_limit: i64) -> String {
    let mut help = format!("Max items to return (client-side, 0=all, default {default_limit}");
    if max_limit > 0 {
        help.push_str(&format!(", max {max_limit}"));
    }
    help.push(')');
    help
}

fn pagination_limit_value_parser(max_limit: i64) -> ValueParser {
    ValueParser::new(move |raw: &str| -> Result<i64, String> {
        let value = raw
            .parse::<i64>()
            .map_err(|_| format!("invalid limit value {raw:?}"))?;
        if max_limit > 0 && value > max_limit {
            return Err(format!("limit {value} exceeds the maximum of {max_limit}"));
        }
        Ok(value)
    })
}

/// Rejects a negative `--offset` at parse time — a `clap` usage error — rather
/// than letting it reach `apply_pagination` in `output/pipeline.rs`, which
/// already rejects one, but only once the command has otherwise fully run.
fn pagination_offset_value_parser() -> ValueParser {
    ValueParser::new(|raw: &str| -> Result<i64, String> {
        let value = raw
            .parse::<i64>()
            .map_err(|_| format!("invalid offset value {raw:?}"))?;
        if value < 0 {
            return Err(format!("offset {value} must be non-negative"));
        }
        Ok(value)
    })
}

pub(crate) fn compat_bool_value_parser() -> ValueParser {
    ValueParser::new(parse_compat_bool)
}

pub(super) fn parse_compat_bool(raw: &str) -> Result<bool, String> {
    match raw {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        _ => Err(format!("invalid boolean value {raw:?}")),
    }
}
