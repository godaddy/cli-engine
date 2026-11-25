//! Per-command settings for the questions the engine asks on a command's behalf.

use std::collections::BTreeMap;

use crate::command::CommandSpec;
use crate::flags::InteractivityPolicy;

/// How the engine words and frames the questions it asks for one command.
///
/// Built from a command's spec with [`PromptOptions::for_spec`]. The default
/// asks the way the engine always has: a warning that required arguments are
/// missing, then each question worded from the argument's `--help` sentence.
/// A guided command skips the warning and starts with its first question.
#[derive(Clone, Debug, Default)]
pub(crate) struct PromptOptions {
    /// Whether the command is a guided flow that prompts by default
    /// ([`InteractivityPolicy::DefaultOnTty`]). Asking is what it is for, so
    /// the engine prints no missing-argument warning and goes straight to the
    /// first question.
    pub(crate) guided: bool,
    /// Question text by arg id, from
    /// [`CommandSpec::with_arg_prompt`](crate::CommandSpec::with_arg_prompt).
    pub(crate) arg_prompts: BTreeMap<String, String>,
    /// Ids of optional args to ask for, from
    /// [`CommandSpec::with_prompted_args`](crate::CommandSpec::with_prompted_args).
    pub(crate) prompted_args: Vec<String>,
}

impl PromptOptions {
    /// Reads the prompting settings from a command's spec.
    #[must_use]
    pub(crate) fn for_spec(spec: &CommandSpec) -> Self {
        Self {
            guided: spec.interactivity == InteractivityPolicy::DefaultOnTty,
            arg_prompts: spec.arg_prompts.clone(),
            prompted_args: spec.prompted_args.clone(),
        }
    }

    /// The question text set for `id`, if any.
    pub(super) fn prompt_for(&self, id: &str) -> Option<&str> {
        self.arg_prompts.get(id).map(String::as_str)
    }
}
