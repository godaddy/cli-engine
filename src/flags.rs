use std::collections::BTreeSet;
use std::io::IsTerminal;

use clap::{Arg, ArgAction, ArgMatches, Command, builder::ValueParser, value_parser};

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
    /// JMESPath per-item filter.
    pub filter: String,
    /// JMESPath whole-result expression.
    pub expr: String,
    /// Client-side page size.
    pub limit: i64,
    /// Client-side page offset.
    pub offset: i64,
    /// Whether schema rendering was requested.
    pub schema: bool,
    /// User-provided command reason.
    pub reason: String,
    /// Raw timeout string.
    pub timeout: String,
    /// Debug selector.
    pub debug: String,
    /// Search query.
    pub search: String,
    /// Credential storage override from `--credential-store`, if supplied.
    pub credential_store: Option<crate::config::CredentialStore>,
}

impl Default for GlobalFlags {
    fn default() -> Self {
        Self {
            output_format: "json".to_owned(),
            verbose: String::new(),
            dry_run: false,
            fields: String::new(),
            filter: String::new(),
            expr: String::new(),
            limit: 0,
            offset: 0,
            schema: false,
            reason: String::new(),
            timeout: "0s".to_owned(),
            debug: String::new(),
            search: String::new(),
            credential_store: None,
        }
    }
}

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
                .help("Print help"),
        )
        .arg(
            Arg::new("output")
                .long("output")
                .short('o')
                .global(true)
                .value_name("FORMAT")
                .default_value("json")
                .help("Output format: toon|json|human"),
        )
        .arg(
            Arg::new("verbose")
                .long("verbose")
                .global(true)
                .num_args(0..=1)
                .default_missing_value("all")
                .value_name("FIELDS")
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
                .help("Preview mutations without executing"),
        )
        .arg(
            Arg::new("fields")
                .long("fields")
                .global(true)
                .value_name("FIELDS")
                .help("Comma-separated fields to include in output (use 'all' or '*' for everything)"),
        )
        .arg(
            Arg::new("filter")
                .long("filter")
                .global(true)
                .value_name("EXPR")
                .help("Per-item JMESPath predicate for list data"),
        )
        .arg(
            Arg::new("expr")
                .long("expr")
                .global(true)
                .value_name("EXPR")
                .help("JMESPath query applied to the whole result"),
        )
        .arg(
            Arg::new("limit")
                .long("limit")
                .global(true)
                .value_parser(value_parser!(i64))
                .allow_hyphen_values(true)
                .default_value("0")
                .help("Max items to return (client-side, 0=all)"),
        )
        .arg(
            Arg::new("offset")
                .long("offset")
                .global(true)
                .value_parser(value_parser!(i64))
                .allow_hyphen_values(true)
                .default_value("0")
                .help("Skip N items before applying limit"),
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
                .help("Dump output field metadata instead of running the command"),
        )
        .arg(
            Arg::new("timeout")
                .long("timeout")
                .global(true)
                .allow_hyphen_values(true)
                .default_value("0s")
                .value_name("DURATION")
                .help("Overall command timeout (e.g. 60s, 5m); default 0s = no timeout"),
        )
        .arg(
            Arg::new("debug")
                .long("debug")
                .global(true)
                .num_args(0..=1)
                .default_missing_value("*")
                .value_name("PATTERN")
                .help("Enable debug logging (comma-separated component patterns, e.g. *, transport, *,-auth)"),
        )
        .arg(
            Arg::new("search")
                .long("search")
                .global(true)
                .value_name("KEYWORD")
                .help("Search commands and guides by keyword"),
        )
        .arg(
            Arg::new("credential-store")
                .long("credential-store")
                .global(true)
                .value_name("MODE")
                .value_parser(|s: &str| s.parse::<crate::config::CredentialStore>())
                .help("Credential storage: auto|keyring|file (overrides env and config)"),
        )
        .arg(
            Arg::new("json")
                .long("json")
                .global(true)
                .action(ArgAction::SetTrue)
                .help("Shorthand for --output json"),
        )
        .arg(
            Arg::new("toon")
                .long("toon")
                .global(true)
                .action(ArgAction::SetTrue)
                .help("Shorthand for --output toon"),
        )
        .arg(
            Arg::new("human")
                .long("human")
                .global(true)
                .action(ArgAction::SetTrue)
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
            .help("Short explanation of why this command is being run (forwarded to your authorizer, auditor, or activity emitter)"),
    )
}

/// Resolves the default output format when the user gave no explicit format.
///
/// Precedence here is env-override first, then a TTY policy: an interactive
/// terminal gets human-friendly output, everything else (pipes, files, CI,
/// most agents) gets machine-readable JSON. Pure so it can be unit-tested
/// without a real terminal.
#[must_use]
pub fn resolve_default_output_format(env_override: Option<&str>, is_tty: bool) -> String {
    if let Some(value) = env_override {
        // Normalize case (env vars are commonly upper/mixed case) and ignore
        // blank or unrecognized values, so a stray or miscased override can't
        // break all command output — only a valid format is honored.
        let normalized = value.trim().to_ascii_lowercase();
        if crate::output::is_valid_output_format(&normalized) {
            return normalized;
        }
    }
    if is_tty { "human" } else { "json" }.to_owned()
}

/// Sanitizes an app id into an environment-variable prefix: ASCII alphanumerics
/// are uppercased and every other character becomes `_`, e.g. `godaddy` ->
/// `GODADDY`, `my-cli` -> `MY_CLI`.
///
/// Shared by the framework's app-scoped env vars (for example
/// [`output_env_var`] and `${PREFIX}_CREDENTIAL_STORE`) so they derive the same
/// prefix from a given app id.
#[must_use]
pub fn app_id_env_prefix(app_id: &str) -> String {
    app_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Derives the per-application output-format override env var from an app id,
/// e.g. `godaddy` -> `GODADDY_OUTPUT`, `gdx` -> `GDX_OUTPUT`.
#[must_use]
pub fn output_env_var(app_id: &str) -> String {
    format!("{}_OUTPUT", app_id_env_prefix(app_id))
}

/// Computes the default output format for `app_id`, consulting the
/// `${APP_ID}_OUTPUT` env override and whether stdout is an interactive
/// terminal. Used as the fallback when no explicit `--output`/`--json`/
/// `--toon`/`--human` is given.
#[must_use]
pub fn default_output_format(app_id: &str) -> String {
    let env = std::env::var(output_env_var(app_id)).ok();
    resolve_default_output_format(env.as_deref(), std::io::stdout().is_terminal())
}

#[must_use]
/// Extracts framework-global flags from parsed `clap` matches, falling back to
/// `default_format` when the user gave no explicit output format.
pub fn global_flags_from_matches(matches: &ArgMatches, default_format: &str) -> GlobalFlags {
    let output_format = if matches.get_flag("toon") {
        "toon".to_owned()
    } else if matches.get_flag("human") {
        "human".to_owned()
    } else if matches.get_flag("json") {
        "json".to_owned()
    } else if matches.value_source("output") == Some(clap::parser::ValueSource::CommandLine) {
        matches
            .get_one::<String>("output")
            .cloned()
            .unwrap_or_else(|| default_format.to_owned())
    } else {
        default_format.to_owned()
    };

    GlobalFlags {
        output_format,
        verbose: matches
            .get_one::<String>("verbose")
            .cloned()
            .unwrap_or_default(),
        dry_run: matches.get_one::<bool>("dry-run").copied().unwrap_or(false),
        fields: matches
            .get_one::<String>("fields")
            .cloned()
            .unwrap_or_default(),
        filter: matches
            .get_one::<String>("filter")
            .cloned()
            .unwrap_or_default(),
        expr: matches
            .get_one::<String>("expr")
            .cloned()
            .unwrap_or_default(),
        limit: matches.get_one::<i64>("limit").copied().unwrap_or(0),
        offset: matches.get_one::<i64>("offset").copied().unwrap_or(0),
        schema: matches.get_one::<bool>("schema").copied().unwrap_or(false),
        // `--reason` is only registered when an authorizer/auditor/activity
        // emitter is configured.
        reason: matches
            .try_get_one::<String>("reason")
            .ok()
            .flatten()
            .cloned()
            .unwrap_or_default(),
        timeout: matches
            .get_one::<String>("timeout")
            .cloned()
            .unwrap_or_else(|| "0s".to_owned()),
        debug: matches
            .get_one::<String>("debug")
            .cloned()
            .unwrap_or_default(),
        search: matches
            .get_one::<String>("search")
            .cloned()
            .unwrap_or_default(),
        credential_store: matches
            .get_one::<crate::config::CredentialStore>("credential-store")
            .copied(),
    }
}

#[must_use]
/// Extracts `--search` from raw args before normal parsing.
pub fn extract_search_query(args: &[impl AsRef<str>]) -> String {
    for index in 0..args.len() {
        let arg = args[index].as_ref();
        if arg == "--search" {
            return args
                .get(index + 1)
                .map_or_else(String::new, |value| value.as_ref().to_owned());
        }
        if let Some(value) = arg.strip_prefix("--search=") {
            return value.to_owned();
        }
    }
    String::new()
}

#[must_use]
/// Extracts output format from raw args.
///
/// Recognizes `--output <format>` / `-o <format>` / `--output=<format>`,
/// plus `--json`, `--toon`, and `--human` as shorthand for their respective
/// formats. Falls back to `default_format` when none is present.
pub fn extract_output_format(args: &[impl AsRef<str>], default_format: &str) -> String {
    for index in 0..args.len() {
        let arg = args[index].as_ref();
        if arg == "--output" || arg == "-o" {
            return args.get(index + 1).map_or_else(
                || default_format.to_owned(),
                |value| value.as_ref().to_owned(),
            );
        }
        if let Some(value) = arg.strip_prefix("--output=") {
            return value.to_owned();
        }
        if arg == "--json" {
            return "json".to_owned();
        }
        if arg == "--toon" {
            return "toon".to_owned();
        }
        if arg == "--human" {
            return "human".to_owned();
        }
    }
    default_format.to_owned()
}

#[must_use]
/// Extracts a colon-separated command path from raw args.
pub fn extract_command_path(
    args: &[impl AsRef<str>],
    bool_flags: &BTreeSet<String>,
    value_flags: &BTreeSet<String>,
) -> String {
    let mut parts = Vec::new();
    let mut index = 1;
    while index < args.len() {
        let arg = args[index].as_ref();
        if arg == "--schema" {
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            if bool_flags.contains(arg) || arg.contains('=') {
                index += 1;
                continue;
            }
            if value_flags.contains(arg)
                || (index + 1 < args.len() && !args[index + 1].as_ref().starts_with('-'))
            {
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }
        parts.push(arg.to_owned());
        index += 1;
    }
    parts.join(":")
}

#[must_use]
/// Reports whether raw args contain a true `--schema` flag.
pub fn has_true_schema_flag(args: &[impl AsRef<str>]) -> bool {
    for arg in args {
        let arg = arg.as_ref();
        if arg == "--schema" {
            return true;
        }
        if let Some(value) = arg.strip_prefix("--schema=") {
            return parse_compat_bool(value).unwrap_or(false);
        }
    }
    false
}

fn compat_bool_value_parser() -> ValueParser {
    ValueParser::new(parse_compat_bool)
}

fn parse_compat_bool(raw: &str) -> Result<bool, String> {
    match raw {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        _ => Err(format!("invalid boolean value {raw:?}")),
    }
}

#[must_use]
/// Derives flag names that do not consume the following token.
pub fn derive_bool_flags(command: &Command) -> BTreeSet<String> {
    let mut flags = BTreeSet::from([
        "--help".to_owned(),
        "-h".to_owned(),
        "--verbose".to_owned(),
        "--debug".to_owned(),
    ]);
    collect_flag_names(command, &mut |arg, name| {
        if !arg_requires_value(arg) {
            flags.insert(name);
        }
    });
    flags
}

#[must_use]
/// Derives flag names that consume the following token.
pub fn derive_value_flags(command: &Command) -> BTreeSet<String> {
    let mut flags = BTreeSet::new();
    collect_flag_names(command, &mut |arg, name| {
        if arg_requires_value(arg) {
            flags.insert(name);
        }
    });
    flags
}

fn collect_flag_names(command: &Command, visit: &mut impl FnMut(&Arg, String)) {
    for arg in command.get_arguments() {
        if arg.is_positional() {
            continue;
        }
        if let Some(long) = arg.get_long() {
            visit(arg, format!("--{long}"));
        }
        if let Some(short) = arg.get_short() {
            visit(arg, format!("-{short}"));
        }
    }
    for child in command.get_subcommands() {
        collect_flag_names(child, visit);
    }
}

/// Reports whether a `--debug` pattern enables a named component.
///
/// The pattern is a comma-separated list of tokens applied left to right, so
/// later tokens override earlier ones:
///
/// - `*` enables every component; `-*` disables every component.
/// - `name` enables that component; `-name` disables it.
/// - whitespace around tokens is ignored and matching is case-insensitive.
///
/// An empty pattern enables nothing. Tokens that name other components are
/// ignored for the queried `component`.
///
/// # Examples
///
/// ```
/// use cli_engine::debug_component_enabled;
///
/// assert!(debug_component_enabled("*", "transport"));
/// assert!(debug_component_enabled("transport", "transport"));
/// assert!(!debug_component_enabled("*,-transport", "transport"));
/// assert!(debug_component_enabled("*,-auth", "transport"));
/// assert!(!debug_component_enabled("", "transport"));
/// ```
#[must_use]
pub fn debug_component_enabled(pattern: &str, component: &str) -> bool {
    let component = component.trim().to_ascii_lowercase();
    // Fail closed: an empty component name is never enabled, not even by `*`.
    if component.is_empty() {
        return false;
    }
    let mut enabled = false;
    for raw in pattern.split(',') {
        let token = raw.trim();
        if token.is_empty() {
            continue;
        }
        let (negated, name) = token
            .strip_prefix('-')
            .map_or((false, token), |rest| (true, rest));
        let name = name.trim().to_ascii_lowercase();
        if name == "*" || name == component {
            enabled = !negated;
        }
    }
    enabled
}

fn arg_requires_value(arg: &Arg) -> bool {
    match arg.get_action() {
        ArgAction::Set | ArgAction::Append => arg
            .get_num_args()
            .is_none_or(|range| range.takes_values() && range.min_values() > 0),
        ArgAction::SetTrue
        | ArgAction::SetFalse
        | ArgAction::Count
        | ArgAction::Help
        | ArgAction::HelpShort
        | ArgAction::HelpLong
        | ArgAction::Version => false,
        _ => arg
            .get_num_args()
            .is_some_and(|range| range.takes_values() && range.min_values() > 0),
    }
}

#[cfg(test)]
mod tests {
    use clap::Command;

    use super::{
        debug_component_enabled, output_env_var, register_global_flags,
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
        // TTY policy when no env override.
        assert_eq!(resolve_default_output_format(None, true), "human");
        assert_eq!(resolve_default_output_format(None, false), "json");
        // A valid env override wins over the TTY policy in both directions.
        assert_eq!(resolve_default_output_format(Some("json"), true), "json");
        assert_eq!(resolve_default_output_format(Some("human"), false), "human");
        // Env override is case-insensitive (env vars are commonly upper-cased).
        assert_eq!(resolve_default_output_format(Some("JSON"), true), "json");
        assert_eq!(
            resolve_default_output_format(Some(" Human "), false),
            "human"
        );
        // Blank or unrecognized env overrides are ignored (fall back to TTY).
        assert_eq!(resolve_default_output_format(Some("   "), false), "json");
        assert_eq!(resolve_default_output_format(Some(""), true), "human");
        assert_eq!(resolve_default_output_format(Some("yaml"), false), "json");
        assert_eq!(resolve_default_output_format(Some("yaml"), true), "human");
    }

    #[test]
    fn output_env_var_is_derived_from_app_id() {
        assert_eq!(output_env_var("godaddy"), "GODADDY_OUTPUT");
        assert_eq!(output_env_var("gdx"), "GDX_OUTPUT");
        assert_eq!(output_env_var("my-cli"), "MY_CLI_OUTPUT");
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
}
