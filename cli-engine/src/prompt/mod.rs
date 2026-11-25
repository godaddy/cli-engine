//! Interactive prompt helpers for CLI commands.
//!
//! These functions wrap [`inquire`] to provide consistent prompts that respect
//! the global interactivity mode. Each helper returns a [`Result`] that
//! produces a user-cancelled error when the user presses Escape or Ctrl+C.
//!
//! All functions require an interactive TTY. Call them only when
//! [`CommandContext::is_interactive`](crate::command::CommandContext::is_interactive)
//! returns `true`.

mod optional;
mod options;
mod prompter;
mod recovery;
mod scripted;
#[cfg(test)]
mod tests;

pub(crate) use optional::{OptionalPromptResult, prompt_optional_args};
pub(crate) use options::PromptOptions;
pub use prompter::{Prompter, TerminalPrompter, TextValidator};
pub use recovery::{CommandCorrection, build_resume_command, confirm_command_correction};
pub(crate) use recovery::{RecoveryResult, try_recover_missing_args};
pub use scripted::{Answer, ScriptedPrompter};

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
    // The returned answer is trimmed, so validate what will actually be
    // returned. Otherwise whitespace alone would pass a non-empty check.
    prompt = prompt.with_validator(move |input: &str| {
        Ok(match (validator)(input.trim()) {
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

/// Asks `first`, and while the answer is yes runs `each` then asks `again`.
///
/// For "add another" flows such as DNS records. `each` does one round of
/// prompting and may fail, which ends the loop with that error. Returns the
/// number of rounds run. Both questions default to no.
///
/// # Errors
///
/// Returns the prompter's error if a question is cancelled, or `each`'s error.
pub fn prompt_repeat(
    prompter: &dyn Prompter,
    first: &str,
    again: &str,
    mut each: impl FnMut(&dyn Prompter) -> crate::Result<()>,
) -> crate::Result<usize> {
    let mut rounds = 0;
    let mut question = first;
    while prompter.confirm(question, false)? {
        each(prompter)?;
        rounds += 1;
        question = again;
    }
    Ok(rounds)
}

/// The error for a command that needs answers it cannot ask for.
///
/// `instead` finishes "Instead, ..." in the fix hint. See
/// [`CommandContext::require_interactive`](crate::command::CommandContext::require_interactive).
#[must_use]
pub fn needs_interactive(instead: &str) -> CliCoreError {
    CliCoreError::with_fix(
        format!(
            "This command asks questions, and prompting is off (there is no terminal, or \
             --non-interactive was passed). Instead, {instead}."
        ),
        CliCoreError::message("cannot prompt for the missing values"),
    )
}

/// Exit code for a prompt the user cancelled (Escape or Ctrl+C), the same code
/// the engine's own missing-argument recovery exits with.
pub const CANCELLED_EXIT_CODE: i32 = 130;

/// Convert an `inquire` error into a CLI-engine error.
fn inquire_error_to_cli(err: inquire::InquireError) -> CliCoreError {
    match err {
        inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted => {
            CliCoreError::with_exit_code(
                CANCELLED_EXIT_CODE,
                CliCoreError::message("prompt cancelled"),
            )
        }
        inquire::InquireError::NotTTY => CliCoreError::with_fix(
            "Run this in a terminal, or pass --non-interactive and supply every value as a flag.",
            CliCoreError::message("cannot prompt: there is no terminal to answer in"),
        ),
        other => CliCoreError::message(format!("prompt error: {other}")),
    }
}
