use std::collections::BTreeSet;
use std::io::IsTerminal;

use clap::ArgMatches;

use super::register::parse_compat_bool;
use super::{GlobalFlags, detect_interactive};

/// Resolves the default output format when the user gave no explicit format.
///
/// Precedence: `env_override`, then `config_override` (the `[output].format`
/// key in `config.toml`), then a TTY policy — an interactive terminal gets
/// human-friendly output, everything else (pipes, files, CI, most agents)
/// gets machine-readable JSON. Pure so it can be unit-tested without a real
/// terminal or config file.
#[must_use]
pub fn resolve_default_output_format(
    env_override: Option<&str>,
    config_override: Option<&str>,
    is_tty: bool,
) -> String {
    // Normalize case (env vars and config values are commonly upper/mixed
    // case) and ignore blank or unrecognized values, so a stray or miscased
    // override can't break all command output — only a valid format is
    // honored, and an invalid one falls through to the next tier.
    for candidate in [env_override, config_override].into_iter().flatten() {
        let normalized = candidate.trim().to_ascii_lowercase();
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

/// Derives the per-application global minimum-stage override env var from an
/// app id, e.g. `godaddy` -> `GODADDY_MIN_STAGE`, `gdx` -> `GDX_MIN_STAGE`.
#[must_use]
pub fn min_stage_env_var(app_id: &str) -> String {
    format!("{}_MIN_STAGE", app_id_env_prefix(app_id))
}

/// Computes the default output format for `app_id`, consulting the
/// `${APP_ID}_OUTPUT` env override, the `[output].format` key in
/// `config.toml`, and whether stdout is an interactive terminal. Used as the
/// fallback when no explicit `--output`/`--json`/`--toon`/`--human` is given.
///
/// **Blocking**: this loads `config.toml` (see
/// [`ConfigFile::load`](crate::config::ConfigFile::load)), performing
/// synchronous filesystem I/O. `Cli` itself never calls this — it resolves
/// the default from the config already loaded once at `Cli::new` time
/// instead — but a consumer calling this function directly should avoid
/// doing so from a hot path or within an async executor without
/// `spawn_blocking`.
#[must_use]
pub fn default_output_format(app_id: &str) -> String {
    let env = std::env::var(output_env_var(app_id)).ok();
    let file = crate::config::load(app_id);
    resolve_default_output_format(
        env.as_deref(),
        file.output.format.as_deref(),
        std::io::stdout().is_terminal(),
    )
}

#[must_use]
/// Extracts framework-global flags from parsed `clap` matches, falling back to
/// `default_format` when the user gave no explicit output format.
pub fn global_flags_from_matches(
    matches: &ArgMatches,
    default_format: &str,
    auto_interactive: bool,
) -> GlobalFlags {
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
        fields_explicit: matches.value_source("fields")
            == Some(clap::parser::ValueSource::CommandLine),
        filter: matches
            .get_one::<String>("filter")
            .cloned()
            .unwrap_or_default(),
        expr: matches
            .get_one::<String>("expr")
            .cloned()
            .unwrap_or_default(),
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
        credential_store: matches
            .get_one::<crate::config::CredentialStore>("credential-store")
            .copied(),
        interactive: if matches.get_flag("non-interactive") {
            false
        } else if matches.get_flag("interactive") {
            true
        } else if auto_interactive {
            detect_interactive()
        } else {
            false
        },
    }
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
