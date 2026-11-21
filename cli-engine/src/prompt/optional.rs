//! Prompting for optional arguments a command asked to be prompted for.

use super::Prompter;
use super::recovery::{
    append_prompted_arg, build_resume_command, format_prompt_message, value_validator,
};

/// Result of asking for optional args: the args to append, or a cancel.
#[derive(Debug)]
pub enum OptionalPromptResult {
    /// Prompting finished; `args` are the tokens to append to the command line
    /// (empty when the user skipped everything).
    Done {
        /// Tokens to append, such as `["--description", "Mail setup"]`.
        args: Vec<String>,
    },
    /// The user cancelled; `resume` is the command to resume with.
    Cancelled {
        /// The command to resume with, including already-supplied flags.
        resume: String,
    },
}

/// Asks for the optional args a command named with
/// [`CommandSpec::with_prompted_args`](crate::CommandSpec::with_prompted_args)
/// that the user has not supplied.
///
/// `matches` are the leaf matches from a successful parse of `supplied`, used
/// to tell which args were given (on the command line or by environment
/// variable). `leaf` is the command's `clap` definition. A blank answer skips
/// the arg. Answers are validated against the arg's own value parser.
pub fn prompt_optional_args(
    prompter: &dyn Prompter,
    root: &clap::Command,
    leaf: &clap::Command,
    matches: &clap::ArgMatches,
    prompted_ids: &[String],
    supplied: &[String],
    app_name: &str,
) -> OptionalPromptResult {
    use clap::parser::ValueSource;

    let mut appended: Vec<String> = Vec::new();
    let mut so_far: Vec<String> = supplied.to_vec();

    for id in prompted_ids {
        let Some(arg) = leaf.get_arguments().find(|a| a.get_id().as_str() == id) else {
            continue;
        };
        if matches!(
            matches.value_source(id),
            Some(ValueSource::CommandLine | ValueSource::EnvVariable)
        ) {
            continue;
        }

        let message = optional_prompt_message(id, arg);
        let answer = match ask_optional(prompter, &message, arg, root, &so_far) {
            Ok(answer) => answer,
            Err(_) => {
                let resume = build_resume_command(app_name, &so_far[1..]);
                return OptionalPromptResult::Cancelled { resume };
            }
        };
        let Some(value) = answer else { continue };

        let start = appended.len();
        append_prompted_arg(&mut appended, arg, &value);
        so_far.extend_from_slice(&appended[start..]);
    }
    OptionalPromptResult::Done { args: appended }
}

/// The prompt text for an optional arg: its help, marked optional.
fn optional_prompt_message(id: &str, arg: &clap::Arg) -> String {
    let base = format_prompt_message(id, Some(arg));
    format!("{} (optional):", base.trim_end_matches(':'))
}

/// Asks for one optional arg; `None` means the user skipped it.
fn ask_optional(
    prompter: &dyn Prompter,
    message: &str,
    arg: &clap::Arg,
    root: &clap::Command,
    supplied: &[String],
) -> crate::Result<Option<String>> {
    if matches!(
        arg.get_action(),
        clap::ArgAction::SetTrue | clap::ArgAction::SetFalse
    ) {
        // A flag: only an explicit yes sets it.
        let yes = prompter.confirm(message, false)?;
        return Ok(yes.then(|| "true".to_owned()));
    }

    let possible: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|pv| !pv.is_hide_set())
        .map(|pv| pv.get_name().to_owned())
        .collect();
    if !possible.is_empty() {
        let mut options = possible.clone();
        options.push("(skip)".to_owned());
        let index = prompter.select(message, &options)?;
        return Ok(possible.get(index).cloned());
    }

    let default = arg
        .get_default_values()
        .first()
        .and_then(|value| value.to_str())
        .map(str::to_owned);
    let validator = value_validator(arg.clone(), root.clone(), supplied.to_vec(), true);
    let answer = prompter.text(message, default.as_deref(), Some(validator))?;
    // Blank skips; so does accepting a default the arg already has.
    if answer.is_empty() || default.as_deref() == Some(answer.as_str()) {
        return Ok(None);
    }
    Ok(Some(answer))
}
