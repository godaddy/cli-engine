//! A [`Prompter`] that replays canned answers, for tests.

use std::collections::VecDeque;
use std::sync::Mutex;

use super::{Prompter, TextValidator, inquire_error_to_cli};
use crate::error::CliCoreError;

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
    answers: Mutex<VecDeque<Answer>>,
    transcript: Mutex<Vec<String>>,
    notices: Mutex<Vec<String>>,
}

impl ScriptedPrompter {
    /// Creates a prompter that gives `answers` in order.
    #[must_use]
    pub fn new(answers: impl IntoIterator<Item = Answer>) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().collect()),
            transcript: Mutex::default(),
            notices: Mutex::default(),
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

    /// Every [`notice`](Prompter::notice) shown so far, in order. Notices are
    /// kept apart from [`transcript`](Self::transcript), which holds questions.
    #[must_use]
    pub fn notices(&self) -> Vec<String> {
        self.notices
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
    fn notice(&self, text: &str) {
        if let Ok(mut lines) = self.notices.lock() {
            lines.push(text.to_owned());
        }
    }

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
