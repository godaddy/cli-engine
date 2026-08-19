//! Interactive prompt helpers for CLI commands.
//!
//! These functions wrap [`inquire`] to provide consistent prompts that respect
//! the global interactivity mode. Each helper returns a [`Result`] that
//! produces a user-cancelled error when the user presses Escape or Ctrl+C.
//!
//! All functions require an interactive TTY. Call them only when
//! [`CommandContext::is_interactive`](crate::command::CommandContext::is_interactive)
//! returns `true`.

use std::io::Write;

use crate::error::CliCoreError;

/// Prompt the user for a free-text string input.
///
/// Returns the trimmed user input, or an error if the user cancelled.
///
/// # Errors
///
/// Returns [`CliCoreError`] if the prompt is cancelled or a terminal error occurs.
pub fn prompt_text(message: &str, default: Option<&str>) -> crate::Result<String> {
    let mut prompt = inquire::Text::new(message);
    if let Some(d) = default {
        prompt = prompt.with_default(d);
    }
    prompt
        .prompt()
        .map(|s| s.trim().to_owned())
        .map_err(inquire_error_to_cli)
}

/// Prompt the user for a free-text string with input validation.
///
/// The `validator` closure should return `Ok(())` if the input is valid, or
/// `Err(message)` with a user-facing explanation if invalid.
///
/// # Errors
///
/// Returns [`CliCoreError`] if the prompt is cancelled or a terminal error occurs.
pub fn prompt_text_with_validation(
    message: &str,
    default: Option<&str>,
    validator: impl Fn(&str) -> Result<(), String> + Clone + 'static,
) -> crate::Result<String> {
    let mut prompt = inquire::Text::new(message);
    if let Some(d) = default {
        prompt = prompt.with_default(d);
    }
    prompt = prompt.with_validator(move |input: &str| {
        Ok(match (validator)(input) {
            Ok(()) => inquire::validator::Validation::Valid,
            Err(msg) => inquire::validator::Validation::Invalid(msg.into()),
        })
    });
    prompt
        .prompt()
        .map(|s| s.trim().to_owned())
        .map_err(inquire_error_to_cli)
}

/// Prompt the user to select one option from a list.
///
/// Returns the index of the selected option.
///
/// # Errors
///
/// Returns [`CliCoreError`] if the prompt is cancelled or a terminal error occurs.
pub fn prompt_select(message: &str, options: &[String]) -> crate::Result<usize> {
    let result = inquire::Select::new(message, options.to_vec())
        .prompt()
        .map_err(inquire_error_to_cli)?;
    options
        .iter()
        .position(|o| o == &result)
        .ok_or_else(|| CliCoreError::message("selected option not found in list"))
}

/// Prompt the user for a yes/no confirmation.
///
/// Returns `true` for yes, `false` for no.
///
/// # Errors
///
/// Returns [`CliCoreError`] if the prompt is cancelled or a terminal error occurs.
pub fn prompt_confirm(message: &str, default: bool) -> crate::Result<bool> {
    inquire::Confirm::new(message)
        .with_default(default)
        .prompt()
        .map_err(inquire_error_to_cli)
}

/// Prompt the user to select multiple options from a list.
///
/// Returns the indices of the selected options. The `defaults` slice
/// indicates which items are pre-selected (by index).
///
/// # Errors
///
/// Returns [`CliCoreError`] if the prompt is cancelled or a terminal error occurs.
pub fn prompt_multi_select(
    message: &str,
    options: &[String],
    defaults: &[bool],
) -> crate::Result<Vec<usize>> {
    let defaults_vec: Vec<bool> = if defaults.len() == options.len() {
        defaults.to_vec()
    } else {
        vec![false; options.len()]
    };

    let selected = inquire::MultiSelect::new(message, options.to_vec())
        .with_default(
            &defaults_vec
                .iter()
                .copied()
                .enumerate()
                .filter_map(|(i, d)| d.then_some(i))
                .collect::<Vec<_>>(),
        )
        .prompt()
        .map_err(inquire_error_to_cli)?;

    Ok(selected
        .iter()
        .filter_map(|s| options.iter().position(|o| o == s))
        .collect())
}

/// Attempt to interactively recover from a clap `MissingRequiredArgument` error.
///
/// When the CLI is running interactively and clap reports missing required
/// arguments, this function prompts the user for each missing value (in
/// declaration order), appends them to the original args, and returns `Some`
/// with the augmented arg list so the caller can re-parse.
///
/// Returns `None` if recovery is not possible (non-interactive or not a
/// missing-arg error). Returns `Some(RecoveryResult::Cancelled { .. })` if
/// the user cancels mid-prompt.
///
/// # Arguments
///
/// * `err` — the clap error from `try_get_matches_from`
/// * `original_args` — the args that were passed to clap
/// * `command` — the root `clap::Command` (for arg introspection)
/// * `app_name` — the CLI binary name (first arg)
pub fn try_recover_missing_args(
    err: &clap::error::Error,
    original_args: &[String],
    command: &clap::Command,
    app_name: &str,
) -> Option<RecoveryResult> {
    use clap::error::{ContextKind, ContextValue, ErrorKind};

    if err.kind() != ErrorKind::MissingRequiredArgument {
        return None;
    }

    // Check interactivity from raw args (clap hasn't fully parsed yet).
    if !is_interactive_from_raw_args(original_args) {
        return None;
    }

    // Extract the missing arg names from clap error context.
    let missing_names = match err.get(ContextKind::InvalidArg)? {
        ContextValue::Strings(names) => names.clone(),
        ContextValue::String(name) => vec![name.clone()],
        _ => return None,
    };

    // Resolve the leaf command from the args to get arg metadata.
    let leaf_command = resolve_leaf_command(command, original_args, app_name)?;

    // Collect prompted values, respecting arg declaration order.
    let mut prompted_args: Vec<String> = Vec::new();
    let mut already_supplied: Vec<String> = original_args.to_vec();

    // Show a clear error header so the user knows why they're being prompted.
    let missing_list: Vec<&str> = missing_names
        .iter()
        .map(|n| n.split_whitespace().next().unwrap_or(n.as_str()))
        .collect();
    drop(writeln!(
        std::io::stderr(),
        "⚠ Missing required argument(s): {}\n",
        missing_list.join(", ")
    ));

    for raw_name in &missing_names {
        // Clap reports missing args as e.g. "--quote-token <TOKEN>"; split on
        // whitespace to isolate the flag portion from the value-name placeholder.
        let flag_portion = raw_name.split_whitespace().next().unwrap_or(raw_name);
        let clean_name = strip_arg_decoration(flag_portion);
        let arg_def = leaf_command.get_arguments().find(|a| {
            a.get_id().as_str() == clean_name
                || a.get_long().is_some_and(|l| l == clean_name)
                || a.get_value_names().is_some_and(|vn| {
                    vn.iter()
                        .any(|v| v.to_ascii_uppercase() == raw_name.trim_matches(['<', '>']))
                })
        });

        let prompt_message = format_prompt_message(raw_name, arg_def);
        let value = match infer_and_prompt(&prompt_message, arg_def) {
            Ok(v) => v,
            Err(_) => {
                // User cancelled — build a resume command hint.
                let resume = build_resume_command(app_name, &already_supplied[1..]);
                return Some(RecoveryResult::Cancelled { resume });
            }
        };

        // Append the prompted value to args.
        let start = prompted_args.len();
        if let Some(arg) = arg_def {
            if let Some(long) = arg.get_long() {
                if matches!(
                    arg.get_action(),
                    clap::ArgAction::SetTrue | clap::ArgAction::SetFalse
                ) {
                    // Boolean flags: clap expects `--flag` alone, no value.
                    if value == "true" {
                        prompted_args.push(format!("--{long}"));
                    }
                } else {
                    prompted_args.push(format!("--{long}"));
                    prompted_args.push(value.clone());
                }
            } else {
                prompted_args.push(value.clone());
            }
        } else {
            prompted_args.push(value.clone());
        }

        // Track all tokens added this iteration for the resume command.
        already_supplied.extend_from_slice(&prompted_args[start..]);
    }

    let mut augmented = original_args.to_vec();
    augmented.extend(prompted_args);
    Some(RecoveryResult::Recovered { args: augmented })
}

/// Result of attempting interactive recovery for missing args.
#[derive(Debug)]
pub enum RecoveryResult {
    /// Successfully prompted for all missing values; `args` has the augmented list.
    Recovered { args: Vec<String> },
    /// User cancelled mid-prompt; `resume` is the command to resume with
    /// already-supplied flags.
    Cancelled { resume: String },
}

/// Determine interactivity from raw args (before full clap parse).
fn is_interactive_from_raw_args(args: &[String]) -> bool {
    if args.iter().any(|a| a == "--non-interactive") {
        return false;
    }
    if args.iter().any(|a| a == "--interactive" || a == "-i") {
        return true;
    }
    crate::flags::detect_interactive()
}

/// Walk the command tree to find the leaf command the user was targeting.
fn resolve_leaf_command<'cmd>(
    root: &'cmd clap::Command,
    args: &[String],
    app_name: &str,
) -> Option<&'cmd clap::Command> {
    let mut current = root;
    for arg in args.iter().skip(1) {
        if arg.starts_with('-') {
            continue;
        }
        if arg == app_name {
            continue;
        }
        if let Some(sub) = current.find_subcommand(arg) {
            current = sub;
        } else {
            break;
        }
    }
    Some(current)
}

/// Infer the prompt type from clap arg metadata and prompt accordingly.
///
/// - If the arg has `possible_values`, use a Select prompt.
/// - If the arg is boolean-like (action is SetTrue/SetFalse), use Confirm.
/// - Otherwise, use a Text prompt.
fn infer_and_prompt(message: &str, arg_def: Option<&clap::Arg>) -> crate::Result<String> {
    if let Some(arg) = arg_def {
        // Check for possible values (enum-like).
        let possible: Vec<String> = arg
            .get_possible_values()
            .iter()
            .filter(|pv| !pv.is_hide_set())
            .map(|pv| pv.get_name().to_owned())
            .collect();

        if !possible.is_empty() {
            let idx = prompt_select(message, &possible)?;
            return Ok(possible[idx].clone());
        }

        // Check for boolean action.
        if matches!(
            arg.get_action(),
            clap::ArgAction::SetTrue | clap::ArgAction::SetFalse
        ) {
            let confirmed = prompt_confirm(message, true)?;
            return Ok(confirmed.to_string());
        }
    }

    // Default: free text input.
    prompt_text(message, None)
}

/// Strip clap decoration (`--`, `<>`, `[]`) from a raw arg identifier,
/// yielding the bare name (e.g. `"--team-name"` → `"team-name"`,
/// `"<domain>"` → `"domain"`).
fn strip_arg_decoration(raw: &str) -> &str {
    raw.trim_start_matches('-')
        .trim_matches(['<', '>', '[', ']'])
}

/// Format a human-friendly prompt message from a raw clap arg identifier.
/// Prepends "Enter" and appends a colon for clarity.
fn format_prompt_message(raw_name: &str, arg_def: Option<&clap::Arg>) -> String {
    let base = if let Some(arg) = arg_def
        && let Some(help) = arg.get_help().map(|s| s.to_string())
    {
        help
    } else {
        strip_arg_decoration(raw_name).replace('-', " ")
    };
    // Capitalize first letter after "Enter ".
    let mut chars = base.chars();
    let capitalized = match chars.next() {
        Some(c) => format!("{}{}", c.to_lowercase(), chars.as_str()),
        None => base.clone(),
    };
    format!("Enter {capitalized}:")
}

/// Build a resume command string from the already-supplied args.
///
/// Shows the user what to run to continue where they left off.
pub fn build_resume_command(app_name: &str, supplied_args: &[String]) -> String {
    let mut parts = vec![app_name.to_owned()];
    parts.extend(supplied_args.iter().cloned());
    parts.join(" ")
}

/// Convert an `inquire` error into a CLI-engine error.
fn inquire_error_to_cli(err: inquire::InquireError) -> CliCoreError {
    match err {
        inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted => {
            CliCoreError::message("prompt cancelled")
        }
        other => CliCoreError::message(format!("prompt error: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_interactive_from_raw_args_non_interactive_flag() {
        let args: Vec<String> = vec![
            "my-cli".into(),
            "project".into(),
            "list".into(),
            "--non-interactive".into(),
        ];
        assert!(!is_interactive_from_raw_args(&args));
    }

    #[test]
    fn is_interactive_from_raw_args_interactive_flag() {
        let args: Vec<String> = vec![
            "my-cli".into(),
            "project".into(),
            "list".into(),
            "--interactive".into(),
        ];
        assert!(is_interactive_from_raw_args(&args));
    }

    #[test]
    fn is_interactive_from_raw_args_short_flag() {
        let args: Vec<String> = vec!["my-cli".into(), "-i".into(), "project".into()];
        assert!(is_interactive_from_raw_args(&args));
    }

    #[test]
    fn format_prompt_message_from_flag_name() {
        let msg = format_prompt_message("--team-name", None);
        assert_eq!(msg, "Enter team name:");
    }

    #[test]
    fn format_prompt_message_from_positional() {
        let msg = format_prompt_message("<domain>", None);
        assert_eq!(msg, "Enter domain:");
    }

    #[test]
    fn format_prompt_message_uses_help_text() {
        let arg = clap::Arg::new("team").long("team").help("Team identifier");
        let msg = format_prompt_message("--team", Some(&arg));
        assert_eq!(msg, "Enter team identifier:");
    }

    #[test]
    fn build_resume_command_with_partial_args() {
        let resume = build_resume_command(
            "gddy",
            &[
                "domain".into(),
                "register".into(),
                "--period".into(),
                "2".into(),
            ],
        );
        assert_eq!(resume, "gddy domain register --period 2");
    }

    #[test]
    fn resolve_leaf_command_walks_subcommands() {
        let root = clap::Command::new("my-cli").subcommand(
            clap::Command::new("project")
                .subcommand(clap::Command::new("list").arg(clap::Arg::new("team").long("team"))),
        );
        let args: Vec<String> = vec![
            "my-cli".into(),
            "project".into(),
            "list".into(),
            "--team".into(),
            "dev".into(),
        ];
        let leaf = resolve_leaf_command(&root, &args, "my-cli");
        assert!(leaf.is_some());
        assert_eq!(leaf.expect("tested").get_name(), "list");
    }

    #[test]
    fn try_recover_returns_none_for_non_missing_arg_error() {
        let cmd = clap::Command::new("test").arg(
            clap::Arg::new("name")
                .long("name")
                .value_parser(["alpha", "beta"]),
        );
        let err = cmd
            .try_get_matches_from(["test", "--name", "invalid"])
            .expect_err("should fail");
        let args: Vec<String> = vec!["test".into(), "--name".into(), "invalid".into()];
        let result = try_recover_missing_args(&err, &args, &clap::Command::new("test"), "test");
        assert!(result.is_none());
    }

    #[test]
    fn try_recover_returns_none_when_non_interactive() {
        // Build a command that knows about --non-interactive (like the real CLI)
        // so clap produces a MissingRequiredArgument error, not UnknownArgument.
        let cmd = clap::Command::new("test")
            .arg(clap::Arg::new("name").long("name").required(true))
            .arg(
                clap::Arg::new("non-interactive")
                    .long("non-interactive")
                    .action(clap::ArgAction::SetTrue),
            );
        let err = cmd
            .try_get_matches_from(["test", "--non-interactive"])
            .expect_err("should fail with missing --name");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        let args: Vec<String> = vec!["test".into(), "--non-interactive".into()];
        let lookup_cmd = clap::Command::new("test")
            .arg(clap::Arg::new("name").long("name").required(true))
            .arg(
                clap::Arg::new("non-interactive")
                    .long("non-interactive")
                    .action(clap::ArgAction::SetTrue),
            );
        let result = try_recover_missing_args(&err, &args, &lookup_cmd, "test");
        assert!(result.is_none());
    }

    #[test]
    fn strip_arg_decoration_handles_clap_flag_with_value_name() {
        // Clap reports missing required flags as "--flag-name <VALUE_NAME>".
        // strip_arg_decoration must work on just the flag portion.
        assert_eq!(strip_arg_decoration("--quote-token"), "quote-token");
        assert_eq!(strip_arg_decoration("<DOMAIN>"), "DOMAIN");
        assert_eq!(strip_arg_decoration("[optional]"), "optional");
        // The split-before-strip pattern used in try_recover_missing_args:
        let raw = "--quote-token <TOKEN>";
        let flag_portion = raw.split_whitespace().next().unwrap_or(raw);
        assert_eq!(strip_arg_decoration(flag_portion), "quote-token");
    }
}
