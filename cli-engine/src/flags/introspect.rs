use std::collections::BTreeSet;

use clap::{Arg, ArgAction, Command};

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
