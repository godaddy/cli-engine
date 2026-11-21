//! Prompting for optional arguments a command asked to be prompted for.

use super::Prompter;
use super::recovery::{
    PromptedTokens, answer_problem, build_resume_command, format_prompt_message, is_cancelled,
    value_validator,
};
use crate::error::CliCoreError;

/// Result of asking for optional args.
#[derive(Debug)]
pub enum OptionalPromptResult {
    /// Prompting finished; `args` is the full argument list with the answers
    /// added, equal to the input when the user skipped everything.
    Done {
        /// The command line to parse again.
        args: Vec<String>,
    },
    /// The user cancelled; `resume` is the command to resume with.
    Cancelled {
        /// The command to resume with, including already-supplied flags.
        resume: String,
    },
    /// A prompt failed for a reason other than the user cancelling, such as
    /// there being no terminal; `error` is the original diagnostic.
    Failed {
        /// The error to render.
        error: CliCoreError,
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

    let mut prompted = PromptedTokens::default();

    for id in prompted_ids {
        let Some(arg) = leaf.get_arguments().find(|a| a.get_id().as_str() == id) else {
            continue;
        };
        // A positional's answer would land in the first free slot rather than
        // its own, so only options can be prompted. Registration asserts this.
        if arg.get_long().is_none() && arg.get_short().is_none() {
            continue;
        }
        if matches!(
            matches.value_source(id),
            Some(ValueSource::CommandLine | ValueSource::EnvVariable)
        ) {
            continue;
        }

        let message = optional_prompt_message(id, arg);
        let so_far = prompted.applied(supplied);
        match ask_optional(prompter, &message, arg, root, &so_far) {
            Ok(Some(value)) => prompted.push(Some(arg), &value),
            Ok(None) => {}
            Err(error) if is_cancelled(&error) => {
                let resume = build_resume_command(app_name, so_far.get(1..).unwrap_or_default());
                return OptionalPromptResult::Cancelled { resume };
            }
            Err(error) => return OptionalPromptResult::Failed { error },
        }
    }
    OptionalPromptResult::Done {
        args: prompted.applied(supplied),
    }
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
    // An answer that conflicts with what the user already supplied would only
    // abort the final parse, so never offer one. When nothing valid is left the
    // arg is simply not asked.
    let offered = |value: &str| answer_problem(arg, root, supplied, value).is_none();

    if matches!(
        arg.get_action(),
        clap::ArgAction::SetTrue | clap::ArgAction::SetFalse
    ) {
        if !offered("true") {
            return Ok(None);
        }
        // A flag: only an explicit yes sets it.
        let yes = prompter.confirm(message, false)?;
        return Ok(yes.then(|| "true".to_owned()));
    }

    let possible: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|pv| !pv.is_hide_set())
        .map(|pv| pv.get_name().to_owned())
        .filter(|name| offered(name))
        .collect();
    let has_menu = arg.get_possible_values().iter().any(|pv| !pv.is_hide_set());
    if has_menu {
        if possible.is_empty() {
            return Ok(None);
        }
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
