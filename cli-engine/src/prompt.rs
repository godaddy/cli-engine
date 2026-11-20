//! Interactive prompt helpers for CLI commands.
//!
//! These functions wrap [`inquire`] to provide consistent prompts that respect
//! the global interactivity mode. Each helper returns a [`Result`] that
//! produces a user-cancelled error when the user presses Escape or Ctrl+C.
//!
//! All functions require an interactive TTY. Call them only when
//! [`CommandContext::is_interactive`](crate::command::CommandContext::is_interactive)
//! returns `true`.

use std::io::Write as _;

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

/// Checks one free-text answer; `Err` carries the message shown to the user,
/// who is then asked again.
pub type TextValidator = std::sync::Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// How a command asks the user questions.
///
/// Handlers get the invocation's prompter from
/// [`CommandContext::prompter`](crate::command::CommandContext::prompter).
/// The default talks to the terminal through `inquire`. Tests install a
/// [`ScriptedPrompter`] with
/// [`CliConfig::with_prompter`](crate::CliConfig::with_prompter) so a prompting
/// command can be run without a terminal.
///
/// Every method fails with a user-cancelled error (exit code
/// [`CANCELLED_EXIT_CODE`]) when the user gives up.
pub trait Prompter: std::fmt::Debug + Send + Sync {
    /// Asks for free text. An empty answer returns an empty string unless a
    /// `validator` rejects it.
    ///
    /// # Errors
    ///
    /// Returns [`CliCoreError`] if the prompt is cancelled or cannot be shown.
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
    /// Returns [`CliCoreError`] if the prompt is cancelled or cannot be shown.
    fn select(&self, message: &str, options: &[String]) -> crate::Result<usize>;

    /// Asks a yes/no question.
    ///
    /// # Errors
    ///
    /// Returns [`CliCoreError`] if the prompt is cancelled or cannot be shown.
    fn confirm(&self, message: &str, default: bool) -> crate::Result<bool>;

    /// Asks the user to pick any number of options and returns their indices.
    ///
    /// # Errors
    ///
    /// Returns [`CliCoreError`] if the prompt is cancelled or cannot be shown.
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

/// One canned answer for a [`ScriptedPrompter`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Answer {
    /// Answers a text prompt. An empty string takes the prompt's default.
    Text(String),
    /// Answers a select prompt with the index of an option.
    Select(usize),
    /// Answers a confirm prompt.
    Confirm(bool),
    /// Answers a multi-select prompt with the indices of the chosen options.
    Multi(Vec<usize>),
    /// The user cancels whichever prompt is asked next.
    Cancel,
}

impl Answer {
    /// Shorthand for [`Answer::Text`].
    #[must_use]
    pub fn text(answer: impl Into<String>) -> Self {
        Self::Text(answer.into())
    }
}

/// A [`Prompter`] that replays a fixed list of answers, for tests.
///
/// It behaves like the terminal prompter: a text answer a validator rejects
/// is recorded, and the next answer is taken as the retry. A prompt that
/// finds the list empty, or an answer of the wrong kind, fails the run with an
/// error saying which prompt it was, so a test cannot silently pass on a
/// prompt it did not plan for.
#[derive(Debug, Default)]
pub struct ScriptedPrompter {
    answers: std::sync::Mutex<std::collections::VecDeque<Answer>>,
    transcript: std::sync::Mutex<Vec<String>>,
}

impl ScriptedPrompter {
    /// Creates a prompter that gives `answers` in order.
    #[must_use]
    pub fn new(answers: impl IntoIterator<Item = Answer>) -> Self {
        Self {
            answers: std::sync::Mutex::new(answers.into_iter().collect()),
            transcript: std::sync::Mutex::default(),
        }
    }

    /// Every message asked so far, in order, with a line for each answer a
    /// validator rejected (`"  rejected: <why>"`).
    #[must_use]
    pub fn transcript(&self) -> Vec<String> {
        self.transcript
            .lock()
            .map(|lines| lines.clone())
            .unwrap_or_default()
    }

    /// Number of answers not yet used.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.answers.lock().map_or(0, |answers| answers.len())
    }

    fn record(&self, line: String) {
        if let Ok(mut lines) = self.transcript.lock() {
            lines.push(line);
        }
    }

    fn next(&self, message: &str, wanted: &str) -> crate::Result<Answer> {
        self.record(message.to_owned());
        let answer = self
            .answers
            .lock()
            .map_err(|_| CliCoreError::message("scripted prompter lock poisoned"))?
            .pop_front();
        match answer {
            Some(Answer::Cancel) => Err(inquire_error_to_cli(
                inquire::InquireError::OperationCanceled,
            )),
            Some(answer) => Ok(answer),
            None => Err(CliCoreError::message(format!(
                "scripted prompter ran out of answers at {message:?} (wanted {wanted})"
            ))),
        }
    }

    fn wrong(message: &str, wanted: &str, got: &Answer) -> CliCoreError {
        CliCoreError::message(format!(
            "scripted prompter: {message:?} wanted {wanted}, but the next answer was {got:?}"
        ))
    }
}

impl Prompter for ScriptedPrompter {
    fn text(
        &self,
        message: &str,
        default: Option<&str>,
        validator: Option<TextValidator>,
    ) -> crate::Result<String> {
        loop {
            let answer = match self.next(message, "text")? {
                Answer::Text(answer) => answer,
                other => return Err(Self::wrong(message, "text", &other)),
            };
            let answer = match (answer.trim(), default) {
                ("", Some(default)) => default.to_owned(),
                (trimmed, _) => trimmed.to_owned(),
            };
            if let Some(validator) = &validator
                && let Err(why) = validator(&answer)
            {
                self.record(format!("  rejected: {why}"));
                continue;
            }
            return Ok(answer);
        }
    }

    fn select(&self, message: &str, options: &[String]) -> crate::Result<usize> {
        match self.next(message, "select")? {
            Answer::Select(index) if index < options.len() => Ok(index),
            Answer::Select(index) => Err(CliCoreError::message(format!(
                "scripted prompter: {message:?} has {} options, not index {index}",
                options.len()
            ))),
            other => Err(Self::wrong(message, "select", &other)),
        }
    }

    fn confirm(&self, message: &str, default: bool) -> crate::Result<bool> {
        match self.next(message, "confirm")? {
            Answer::Confirm(answer) => Ok(answer),
            Answer::Text(answer) if answer.is_empty() => Ok(default),
            other => Err(Self::wrong(message, "confirm", &other)),
        }
    }

    fn multi_select(
        &self,
        message: &str,
        options: &[String],
        _defaults: &[bool],
    ) -> crate::Result<Vec<usize>> {
        match self.next(message, "multi-select")? {
            Answer::Multi(indices) if indices.iter().all(|index| *index < options.len()) => {
                Ok(indices)
            }
            other => Err(Self::wrong(message, "multi-select", &other)),
        }
    }
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
fn is_interactive_from_raw_args(args: &[String], auto_interactive: bool) -> bool {
    if args.iter().any(|a| a == "--non-interactive") {
        return false;
    }
    if args.iter().any(|a| a == "--interactive") {
        return true;
    }
    auto_interactive && crate::flags::detect_interactive()
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
/// - Otherwise, use a Text prompt that rejects values clap would reject.
fn infer_and_prompt(
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
fn value_validator(
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
fn first_line(rendered: &str) -> String {
    let line = rendered.lines().next().unwrap_or_default();
    line.trim_start_matches("error:").trim().to_owned()
}

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

/// Strip clap decoration from a single token (`--team-name` → `team-name`,
/// `<domain>` → `domain`). Does not mangle clap's `flag <VALUE>` form.
fn strip_arg_decoration(raw: &str) -> &str {
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
fn missing_arg_lookup_key(raw: &str) -> &str {
    let stripped = raw.trim_start_matches('-');
    strip_arg_decoration(stripped.split_whitespace().next().unwrap_or(stripped))
}

fn find_arg_def<'cmd>(command: &'cmd clap::Command, raw_name: &str) -> Option<&'cmd clap::Arg> {
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

fn format_missing_arg_label(raw_name: &str, arg_def: Option<&clap::Arg>) -> String {
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

fn extract_value_name_suffix(raw_name: &str) -> Option<String> {
    let stripped = raw_name.trim_start_matches('-');
    let (_, value) = stripped.split_once(' ')?;
    Some(strip_arg_decoration(value).to_owned())
}

/// Split clap's combined missing-arg group identifier into individual alternatives.
///
/// Clap reports required `ArgGroup`s as a single token such as
/// `<--a <a>|--b <b>>` or `<--one|--two>`.
fn split_missing_group_alternatives(raw_name: &str) -> Vec<String> {
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

fn format_missing_group_label(raw_name: &str, command: &clap::Command) -> String {
    let labels: Vec<String> = split_missing_group_alternatives(raw_name)
        .iter()
        .map(|alt| format_missing_arg_label(alt, find_arg_def(command, alt)))
        .collect();
    format!("one of: {}", labels.join(", "))
}

fn append_prompted_arg(prompted_args: &mut Vec<String>, arg: &clap::Arg, value: &str) {
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
fn normalize_prompt_base(text: &str) -> String {
    let trimmed = text.trim_end_matches('.').trim_end();
    if trimmed.ends_with(':') {
        trimmed.to_owned()
    } else {
        format!("{trimmed}:")
    }
}

/// Format a human-friendly prompt message from a raw clap arg identifier.
fn format_prompt_message(raw_name: &str, arg_def: Option<&clap::Arg>) -> String {
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
        // --non-interactive wins even if auto_interactive is true
        assert!(!is_interactive_from_raw_args(&args, true));
    }

    #[test]
    fn is_interactive_from_raw_args_interactive_flag() {
        let args: Vec<String> = vec![
            "my-cli".into(),
            "project".into(),
            "list".into(),
            "--interactive".into(),
        ];
        // --interactive works even without auto_interactive
        assert!(is_interactive_from_raw_args(&args, false));
    }

    #[test]
    fn is_interactive_no_flags_auto_disabled() {
        let args: Vec<String> = vec!["my-cli".into(), "project".into(), "list".into()];
        // Without auto_interactive and no explicit flag, never interactive
        assert!(!is_interactive_from_raw_args(&args, false));
    }

    #[test]
    fn format_prompt_message_from_flag_name() {
        let msg = format_prompt_message("--team-name", None);
        assert_eq!(msg, "team name:");
    }

    #[test]
    fn format_prompt_message_from_positional() {
        let msg = format_prompt_message("<domain>", None);
        assert_eq!(msg, "domain:");
    }

    #[test]
    fn strip_arg_decoration_preserves_flag_value_name_pairs() {
        assert_eq!(strip_arg_decoration("tld <TLD>"), "tld <TLD>");
    }

    #[test]
    fn missing_arg_lookup_key_extracts_flag_id() {
        assert_eq!(missing_arg_lookup_key("tld <TLD>"), "tld");
        assert_eq!(missing_arg_lookup_key("--team-name"), "team-name");
        assert_eq!(missing_arg_lookup_key("<domain>"), "domain");
    }

    #[test]
    fn format_prompt_message_from_flag_value_name_pair() {
        let msg = format_prompt_message("tld <TLD>", None);
        assert_eq!(msg, "TLD:");
    }

    #[test]
    fn format_prompt_message_uses_help_text() {
        let arg = clap::Arg::new("team").long("team").help("Team identifier");
        let msg = format_prompt_message("--team", Some(&arg));
        assert_eq!(msg, "Team identifier:");
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
    fn confirm_command_correction_declines_when_non_interactive() {
        let args: Vec<String> = vec!["my-cli".into(), "projet".into()];
        assert_eq!(
            confirm_command_correction(&args, "project", false),
            CommandCorrection::Declined
        );

        let args: Vec<String> = vec!["my-cli".into(), "projet".into(), "--non-interactive".into()];
        assert_eq!(
            confirm_command_correction(&args, "project", true),
            CommandCorrection::Declined
        );
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
        let result =
            try_recover_missing_args(&err, &args, &clap::Command::new("test"), "test", true);
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
        let result = try_recover_missing_args(&err, &args, &lookup_cmd, "test", true);
        assert!(result.is_none());
    }

    #[test]
    fn try_recover_returns_none_when_auto_interactive_disabled() {
        let cmd =
            clap::Command::new("test").arg(clap::Arg::new("name").long("name").required(true));
        let err = cmd
            .try_get_matches_from(["test"])
            .expect_err("should fail with missing --name");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        let args: Vec<String> = vec!["test".into()];
        let lookup_cmd =
            clap::Command::new("test").arg(clap::Arg::new("name").long("name").required(true));
        // auto_interactive = false, no explicit --interactive flag → no recovery
        let result = try_recover_missing_args(&err, &args, &lookup_cmd, "test", false);
        assert!(result.is_none());
    }

    #[allow(clippy::panic)]
    fn missing_arg_names(cmd: &clap::Command, argv: &[&str]) -> Vec<String> {
        use clap::error::{ContextKind, ContextValue, ErrorKind};
        let err = cmd
            .clone()
            .try_get_matches_from(argv)
            .expect_err("expected missing required argument");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
        match err.get(ContextKind::InvalidArg) {
            Some(ContextValue::Strings(names)) => names.clone(),
            Some(ContextValue::String(name)) => vec![name.clone()],
            other => panic!("unexpected InvalidArg context: {other:?}"),
        }
    }

    #[test]
    fn clap_reports_flag_value_name_pair_for_missing_flag_value() {
        let cmd = clap::Command::new("agreements").arg(
            clap::Arg::new("tld")
                .long("tld")
                .value_name("TLD")
                .required(true),
        );
        let names = missing_arg_names(&cmd, &["agreements"]);
        assert_eq!(names, vec!["--tld <TLD>"]);
        let arg_def = find_arg_def(&cmd, &names[0]);
        assert!(arg_def.is_some());
        assert_eq!(format_prompt_message(&names[0], arg_def), "TLD:");
        assert_eq!(format_missing_arg_label(&names[0], arg_def), "--tld");
    }

    #[test]
    fn clap_reports_positional_value_name_for_missing_positional() {
        let cmd = clap::Command::new("suggest").arg(
            clap::Arg::new("query")
                .value_name("QUERY")
                .help("Seed domain or keywords to base suggestions on")
                .required(true),
        );
        let names = missing_arg_names(&cmd, &["suggest"]);
        assert_eq!(names, vec!["<QUERY>"]);
        let arg_def = find_arg_def(&cmd, &names[0]);
        assert!(arg_def.is_some());
        assert_eq!(
            format_prompt_message(&names[0], arg_def),
            "Seed domain or keywords to base suggestions on:"
        );
        assert_eq!(
            format_missing_arg_label(&names[0], arg_def),
            "Seed domain or keywords to base suggestions on"
        );
    }

    #[test]
    fn clap_reports_flag_value_name_pair_with_dashed_long_flag() {
        let cmd = clap::Command::new("test").arg(
            clap::Arg::new("team_name")
                .long("team-name")
                .value_name("TEAM")
                .required(true),
        );
        let names = missing_arg_names(&cmd, &["test"]);
        assert_eq!(names, vec!["--team-name <TEAM>"]);
        let arg_def = find_arg_def(&cmd, &names[0]);
        assert!(arg_def.is_some());
        assert_eq!(format_missing_arg_label(&names[0], arg_def), "--team-name");
        assert_eq!(format_prompt_message(&names[0], arg_def), "TEAM:");
    }

    #[test]
    fn format_prompt_message_handles_legacy_flag_value_name_without_dashes() {
        // Older clap versions reported `tld <TLD>` without a `--` prefix.
        let cmd = clap::Command::new("agreements").arg(
            clap::Arg::new("tld")
                .long("tld")
                .value_name("TLD")
                .required(true),
        );
        let arg_def = find_arg_def(&cmd, "tld <TLD>");
        assert!(arg_def.is_some());
        assert_eq!(format_prompt_message("tld <TLD>", arg_def), "TLD:");
    }

    #[test]
    fn format_prompt_message_avoids_double_colon_when_help_ends_with_colon() {
        let arg = clap::Arg::new("domain")
            .long("domain")
            .help("Enter domain:");
        let msg = format_prompt_message("--domain", Some(&arg));
        assert_eq!(msg, "Enter domain:");
    }

    #[test]
    fn find_arg_def_prefers_the_exact_flag_over_one_sharing_its_value_name() {
        let cmd = clap::Command::new("test")
            .arg(
                clap::Arg::new("provider-name")
                    .long("provider-name")
                    .value_name("NAME")
                    .required(true),
            )
            .arg(
                clap::Arg::new("service-name")
                    .long("service-name")
                    .value_name("NAME")
                    .required(true),
            );
        let found = find_arg_def(&cmd, "--service-name <NAME>").expect("arg");
        assert_eq!(found.get_id().as_str(), "service-name");
        let found = find_arg_def(&cmd, "--provider-name <NAME>").expect("arg");
        assert_eq!(found.get_id().as_str(), "provider-name");
    }

    #[test]
    fn find_arg_def_matches_short_flag_value_name_pair() {
        let cmd = clap::Command::new("test").arg(
            clap::Arg::new("tld")
                .short('t')
                .long("tld")
                .value_name("TLD")
                .required(true),
        );
        let arg_def = find_arg_def(&cmd, "t <TLD>");
        assert!(arg_def.is_some());
        assert_eq!(format_missing_arg_label("t <TLD>", arg_def), "--tld");
    }

    #[test]
    fn required_arg_group_lists_member_flags_in_missing_context() {
        use clap::ArgGroup;
        let cmd = clap::Command::new("update")
            .arg(clap::Arg::new("a").long("a"))
            .arg(clap::Arg::new("b").long("b"))
            .group(ArgGroup::new("ab").args(["a", "b"]).required(true));
        let names = missing_arg_names(&cmd, &["update"]);
        assert_eq!(names.len(), 1);
        assert!(names[0].contains('|'));
        let alternatives = split_missing_group_alternatives(&names[0]);
        assert_eq!(alternatives, vec!["--a <a>", "--b <b>"]);
        assert!(find_arg_def(&cmd, &alternatives[0]).is_some());
        assert!(find_arg_def(&cmd, &alternatives[1]).is_some());
    }

    #[test]
    fn derive_exclusive_group_reports_alternatives() {
        use clap::CommandFactory;

        #[derive(clap::Parser)]
        #[command(name = "bump")]
        struct Bump {
            #[command(flatten)]
            args: ExclusiveArgs,
        }

        #[derive(clap::Args)]
        #[group(required = true, multiple = false)]
        struct ExclusiveArgs {
            #[arg(long)]
            one: bool,
            #[arg(long)]
            two: bool,
        }

        let names = missing_arg_names(&Bump::command(), &["bump"]);
        assert_eq!(names.len(), 1);
        let alternatives = split_missing_group_alternatives(&names[0]);
        assert_eq!(alternatives, vec!["--one", "--two"]);
    }
}
