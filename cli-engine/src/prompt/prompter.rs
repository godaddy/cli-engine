//! The [`Prompter`] abstraction and its terminal implementation.

use std::io::Write as _;
use std::sync::Arc;

use super::{
    prompt_confirm, prompt_multi_select, prompt_select, prompt_text, prompt_text_with_validation,
};

/// Checks one free-text answer; `Err` carries the message shown to the user,
/// who is then asked again.
pub type TextValidator = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// How a command asks the user questions.
///
/// Handlers get the invocation's prompter from
/// [`CommandContext::prompter`](crate::command::CommandContext::prompter).
/// The default talks to the terminal through `inquire`. Tests install a
/// [`ScriptedPrompter`](crate::prompt::ScriptedPrompter) with
/// [`CliConfig::with_prompter`](crate::CliConfig::with_prompter) so a prompting
/// command can be run without a terminal.
///
/// Every method fails with a user-cancelled error (exit code
/// [`CANCELLED_EXIT_CODE`](crate::prompt::CANCELLED_EXIT_CODE)) when the user gives up.
pub trait Prompter: std::fmt::Debug + Send + Sync {
    /// Shows a short heads-up before a run of questions.
    ///
    /// The default writes it to stderr, where the terminal prompts appear, after
    /// a blank line. A prompter overrides this to capture or restyle it; a test
    /// prompter such as [`ScriptedPrompter`](crate::prompt::ScriptedPrompter)
    /// records it instead.
    fn notice(&self, text: &str) {
        drop(writeln!(std::io::stderr(), "\n  {text}"));
    }

    /// Asks for free text. An empty answer returns an empty string unless a
    /// `validator` rejects it.
    ///
    /// # Errors
    ///
    /// Returns [`CliCoreError`](crate::CliCoreError) if the prompt is cancelled or cannot be shown.
    fn text(
        &self,
        message: &str,
        default: Option<&str>,
        validator: Option<TextValidator>,
    ) -> crate::Result<String>;

    /// Asks the user to pick one option and returns its index.
    ///
    /// # Errors
    ///
    /// Returns [`CliCoreError`](crate::CliCoreError) if the prompt is cancelled or cannot be shown.
    fn select(&self, message: &str, options: &[String]) -> crate::Result<usize>;

    /// Asks a yes/no question.
    ///
    /// # Errors
    ///
    /// Returns [`CliCoreError`](crate::CliCoreError) if the prompt is cancelled or cannot be shown.
    fn confirm(&self, message: &str, default: bool) -> crate::Result<bool>;

    /// Asks the user to pick any number of options and returns their indices.
    ///
    /// # Errors
    ///
    /// Returns [`CliCoreError`](crate::CliCoreError) if the prompt is cancelled or cannot be shown.
    fn multi_select(
        &self,
        message: &str,
        options: &[String],
        defaults: &[bool],
    ) -> crate::Result<Vec<usize>>;
}

/// The default [`Prompter`]: prompts on the terminal with `inquire`.
#[derive(Clone, Copy, Debug, Default)]
pub struct TerminalPrompter;

impl Prompter for TerminalPrompter {
    fn text(
        &self,
        message: &str,
        default: Option<&str>,
        validator: Option<TextValidator>,
    ) -> crate::Result<String> {
        match validator {
            Some(validator) => {
                prompt_text_with_validation(message, default, move |input| validator(input))
            }
            None => prompt_text(message, default),
        }
    }

    fn select(&self, message: &str, options: &[String]) -> crate::Result<usize> {
        prompt_select(message, options)
    }

    fn confirm(&self, message: &str, default: bool) -> crate::Result<bool> {
        prompt_confirm(message, default)
    }

    fn multi_select(
        &self,
        message: &str,
        options: &[String],
        defaults: &[bool],
    ) -> crate::Result<Vec<usize>> {
        prompt_multi_select(message, options, defaults)
    }
}
