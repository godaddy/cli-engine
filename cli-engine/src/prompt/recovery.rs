//! Interactive recovery from clap's missing-required-argument error, and the
//! helpers that read a clap command's argument metadata to build prompts.

use std::io::Write as _;

use super::{Prompter, TerminalPrompter, TextValidator, prompt_confirm};

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
/// * `auto_interactive` — whether the CLI opted into TTY auto-detection
pub fn try_recover_missing_args(
    err: &clap::error::Error,
    original_args: &[String],
    command: &clap::Command,
    app_name: &str,
    auto_interactive: bool,
) -> Option<RecoveryResult> {
    try_recover_missing_args_with(
        &TerminalPrompter,
        err,
        original_args,
        command,
        app_name,
        auto_interactive,
    )
}

/// Like [`try_recover_missing_args`], asking through `prompter` instead of the
/// terminal.
///
/// A text answer is checked against the argument's own value parser, so a
/// value clap would reject is explained and asked again instead of failing
/// the re-parse. The CLI passes the prompter set with
/// [`CliConfig::with_prompter`](crate::CliConfig::with_prompter).
pub fn try_recover_missing_args_with(
    prompter: &dyn Prompter,
    err: &clap::error::Error,
    original_args: &[String],
    command: &clap::Command,
    app_name: &str,
    auto_interactive: bool,
) -> Option<RecoveryResult> {
    use clap::error::{ContextKind, ContextValue, ErrorKind};

    if err.kind() != ErrorKind::MissingRequiredArgument {
        return None;
    }

    // Check interactivity from raw args (clap hasn't fully parsed yet).
    if !is_interactive_from_raw_args(original_args, auto_interactive) {
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

    // Print a header so the user knows why they're being prompted.
    let missing_list: Vec<String> = missing_names
        .iter()
        .map(|n| {
            if n.contains('|') {
                format_missing_group_label(n, leaf_command)
            } else {
                format_missing_arg_label(n, find_arg_def(leaf_command, n))
            }
        })
        .collect();
    drop(writeln!(
        std::io::stderr(),
        "\n  \u{26a0} missing required argument(s): {}",
        missing_list.join(", ")
    ));

    // Collect prompted values, respecting arg declaration order.
    let mut prompted_args: Vec<String> = Vec::new();
    let mut already_supplied: Vec<String> = original_args.to_vec();

    for raw_name in &missing_names {
        let selected_raw = if raw_name.contains('|') {
            let alternatives = split_missing_group_alternatives(raw_name);
            let labels: Vec<String> = alternatives
                .iter()
                .map(|alt| format_missing_arg_label(alt, find_arg_def(leaf_command, alt)))
                .collect();
            match prompter.select("Choose one of the required options:", &labels) {
                Ok(idx) => alternatives[idx].clone(),
                Err(_) => {
                    let resume = build_resume_command(app_name, &already_supplied[1..]);
                    return Some(RecoveryResult::Cancelled { resume });
                }
            }
        } else {
            raw_name.clone()
        };

        let arg_def = find_arg_def(leaf_command, &selected_raw);

        if let Some(arg) = arg_def
            && matches!(
                arg.get_action(),
                clap::ArgAction::SetTrue | clap::ArgAction::SetFalse
            )
        {
            // Boolean flag chosen from a required group — no value prompt needed.
            let start = prompted_args.len();
            if let Some(long) = arg.get_long() {
                prompted_args.push(format!("--{long}"));
            }
            already_supplied.extend_from_slice(&prompted_args[start..]);
            continue;
        }

        let prompt_message = format_prompt_message(&selected_raw, arg_def);
        let value = match infer_and_prompt(
            prompter,
            &prompt_message,
            arg_def,
            command,
            &already_supplied,
        ) {
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
            append_prompted_arg(&mut prompted_args, arg, &value);
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

/// Outcome of a "did you mean X?" correction prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandCorrection {
    /// User accepted; re-dispatch with the corrected token.
    Accepted,
    /// Don't rewrite args; caller renders the error (with hint when applicable).
    Declined,
    /// User aborted (Escape or Ctrl+C).
    Cancelled,
}

/// Offer to correct an unknown command spelling. Never prompts when non-interactive;
/// interactivity follows the same raw-args rules as [`try_recover_missing_args`].
pub fn confirm_command_correction(
    args: &[String],
    suggestion: &str,
    auto_interactive: bool,
) -> CommandCorrection {
    if !is_interactive_from_raw_args(args, auto_interactive) {
        return CommandCorrection::Declined;
    }
    match prompt_confirm(&format!("Did you mean `{suggestion}`?"), true) {
        Ok(true) => CommandCorrection::Accepted,
        Ok(false) => CommandCorrection::Declined,
        Err(_) => CommandCorrection::Cancelled,
    }
}

/// Result of attempting interactive recovery for missing args.
#[derive(Debug)]
pub enum RecoveryResult {
    /// Successfully prompted for all missing values; `args` has the augmented list.
    Recovered {
        /// The augmented argument list, with prompted values filled in.
        args: Vec<String>,
    },
    /// User cancelled mid-prompt; `resume` is the command to resume with
    /// already-supplied flags.
    Cancelled {
        /// The command to resume with, including already-supplied flags.
        resume: String,
    },
}

/// Determine interactivity from raw args (before full clap parse).
///
/// Explicit flags always win. When neither is present, falls back to TTY
/// auto-detection only if the CLI opted in via `auto_interactive`.
pub(super) fn is_interactive_from_raw_args(args: &[String], auto_interactive: bool) -> bool {
    if args.iter().any(|a| a == "--non-interactive") {
        return false;
    }
    if args.iter().any(|a| a == "--interactive") {
        return true;
    }
    auto_interactive && crate::flags::detect_interactive()
}

/// Walk the command tree to find the leaf command the user was targeting.
pub(super) fn resolve_leaf_command<'cmd>(
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
/// - Otherwise, use a Text prompt that rejects values clap would reject.
pub(super) fn infer_and_prompt(
    prompter: &dyn Prompter,
    message: &str,
    arg_def: Option<&clap::Arg>,
    root: &clap::Command,
    supplied: &[String],
) -> crate::Result<String> {
    if let Some(arg) = arg_def {
        // Check for possible values (enum-like).
        let possible: Vec<String> = arg
            .get_possible_values()
            .iter()
            .filter(|pv| !pv.is_hide_set())
            .map(|pv| pv.get_name().to_owned())
            .collect();

        if !possible.is_empty() {
            let idx = prompter.select(message, &possible)?;
            return Ok(possible[idx].clone());
        }

        // Check for boolean action.
        if matches!(
            arg.get_action(),
            clap::ArgAction::SetTrue | clap::ArgAction::SetFalse
        ) {
            let confirmed = prompter.confirm(message, true)?;
            return Ok(confirmed.to_string());
        }

        let validator = value_validator(arg.clone(), root.clone(), supplied.to_vec(), false);
        return prompter.text(message, None, Some(validator));
    }

    // Default: free text input.
    prompter.text(message, None, None)
}

/// Builds a validator that asks clap whether `arg` accepts the typed value.
///
/// It re-parses the supplied arguments plus the candidate and keeps only the
/// errors about the candidate itself. Other errors, such as further required
/// arguments still missing, are not the user's mistake in this answer.
pub(super) fn value_validator(
    arg: clap::Arg,
    root: clap::Command,
    supplied: Vec<String>,
    allow_blank: bool,
) -> TextValidator {
    use clap::error::ErrorKind;

    std::sync::Arc::new(move |input: &str| {
        if input.is_empty() {
            return if allow_blank {
                Ok(())
            } else {
                Err("A value is required.".to_owned())
            };
        }
        let mut tokens = Vec::new();
        append_prompted_arg(&mut tokens, &arg, input);
        let mut args = supplied.clone();
        args.extend(tokens);
        match root.clone().try_get_matches_from(args) {
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ValueValidation | ErrorKind::InvalidValue | ErrorKind::InvalidUtf8
                ) =>
            {
                Err(first_line(&error.render().to_string()))
            }
            _ => Ok(()),
        }
    })
}

/// The first line of a clap error, without its `error:` prefix.
pub(super) fn first_line(rendered: &str) -> String {
    let line = rendered.lines().next().unwrap_or_default();
    line.trim_start_matches("error:").trim().to_owned()
}

/// Strip clap decoration from a single token (`--team-name` → `team-name`,
/// `<domain>` → `domain`). Does not mangle clap's `flag <VALUE>` form.
pub(super) fn strip_arg_decoration(raw: &str) -> &str {
    let stripped = raw.trim_start_matches('-');
    if stripped.starts_with('<') && stripped.ends_with('>') && stripped.len() > 2 {
        return &stripped[1..stripped.len() - 1];
    }
    if stripped.starts_with('[') && stripped.ends_with(']') && stripped.len() > 2 {
        return &stripped[1..stripped.len() - 1];
    }
    stripped
}

/// Lookup key for a clap missing-arg identifier (`tld <TLD>` → `tld`).
pub(super) fn missing_arg_lookup_key(raw: &str) -> &str {
    let stripped = raw.trim_start_matches('-');
    strip_arg_decoration(stripped.split_whitespace().next().unwrap_or(stripped))
}

pub(super) fn find_arg_def<'cmd>(
    command: &'cmd clap::Command,
    raw_name: &str,
) -> Option<&'cmd clap::Arg> {
    let clean_name = missing_arg_lookup_key(raw_name);
    // An exact id or long-name match wins. Matching on the value name is only
    // a fallback, because unrelated flags often share one (`--provider-name
    // <NAME>` and `--service-name <NAME>`), and taking the first such flag
    // would prompt for, and set, the wrong one.
    command
        .get_arguments()
        .find(|a| {
            a.get_id().as_str() == clean_name || a.get_long().is_some_and(|l| l == clean_name)
        })
        .or_else(|| {
            command.get_arguments().find(|a| {
                a.get_value_names().is_some_and(|vn| {
                    vn.iter().any(|v| {
                        v.eq_ignore_ascii_case(strip_arg_decoration(
                            raw_name.split_whitespace().nth(1).unwrap_or(raw_name),
                        ))
                    })
                })
            })
        })
}

pub(super) fn format_missing_arg_label(raw_name: &str, arg_def: Option<&clap::Arg>) -> String {
    if let Some(arg) = arg_def {
        if let Some(long) = arg.get_long() {
            return format!("--{long}");
        }
        if let Some(help) = arg.get_help() {
            return help.to_string().trim_end_matches('.').to_owned();
        }
    }
    if let Some(value_name) = extract_value_name_suffix(raw_name) {
        return value_name;
    }
    missing_arg_lookup_key(raw_name).replace('-', " ")
}

pub(super) fn extract_value_name_suffix(raw_name: &str) -> Option<String> {
    let stripped = raw_name.trim_start_matches('-');
    let (_, value) = stripped.split_once(' ')?;
    Some(strip_arg_decoration(value).to_owned())
}

/// Split clap's combined missing-arg group identifier into individual alternatives.
///
/// Clap reports required `ArgGroup`s as a single token such as
/// `<--a <a>|--b <b>>` or `<--one|--two>`.
pub(super) fn split_missing_group_alternatives(raw_name: &str) -> Vec<String> {
    let trimmed = raw_name.trim();
    let inner = trimmed
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(trimmed);
    if !inner.contains('|') {
        return vec![raw_name.to_owned()];
    }
    inner
        .split('|')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(super) fn format_missing_group_label(raw_name: &str, command: &clap::Command) -> String {
    let labels: Vec<String> = split_missing_group_alternatives(raw_name)
        .iter()
        .map(|alt| format_missing_arg_label(alt, find_arg_def(command, alt)))
        .collect();
    format!("one of: {}", labels.join(", "))
}

pub(super) fn append_prompted_arg(prompted_args: &mut Vec<String>, arg: &clap::Arg, value: &str) {
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
            prompted_args.push(value.to_owned());
        }
    } else {
        prompted_args.push(value.to_owned());
    }
}

/// Normalize prompt/help text and append a single trailing colon.
pub(super) fn normalize_prompt_base(text: &str) -> String {
    let trimmed = text.trim_end_matches('.').trim_end();
    if trimmed.ends_with(':') {
        trimmed.to_owned()
    } else {
        format!("{trimmed}:")
    }
}

/// Format a human-friendly prompt message from a raw clap arg identifier.
pub(super) fn format_prompt_message(raw_name: &str, arg_def: Option<&clap::Arg>) -> String {
    let base = if let Some(arg) = arg_def
        && let Some(help) = arg.get_help().map(|s| s.to_string())
    {
        help.trim_end_matches('.').to_owned()
    } else if let Some(value_name) = extract_value_name_suffix(raw_name) {
        value_name
    } else {
        missing_arg_lookup_key(raw_name).replace('-', " ")
    };
    normalize_prompt_base(&base)
}

/// Build a resume command string from the already-supplied args.
///
/// Shows the user what to run to continue where they left off.
pub fn build_resume_command(app_name: &str, supplied_args: &[String]) -> String {
    let mut parts = vec![app_name.to_owned()];
    parts.extend(supplied_args.iter().cloned());
    parts.join(" ")
}
