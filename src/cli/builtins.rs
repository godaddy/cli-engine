use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::Value;

use crate::{
    command::leaf_matches,
    middleware::{ValueMap, value_map},
};

pub(crate) fn guide_command() -> Command {
    Command::new("guide")
        .about("Show built-in guides for AI agents and developers")
        .long_about("Embedded documentation that ships with the binary. Run without arguments to list topics, or specify a topic name.")
        .arg(Arg::new("topic").value_name("topic").num_args(0..=1))
}

pub(crate) fn help_command() -> Command {
    Command::new("help")
        .about("Help about any command")
        .arg(Arg::new("command").value_name("command").num_args(0..))
}

pub(crate) fn help_args(matches: &ArgMatches) -> ValueMap {
    let leaf = leaf_matches(matches);
    let parts = leaf
        .get_many::<String>("command")
        .map(|values| values.map(String::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    if parts.is_empty() {
        return ValueMap::new();
    }
    value_map([("command", Value::String(parts.join(" ")))])
}

pub(crate) fn guide_args(matches: &ArgMatches) -> ValueMap {
    let leaf = leaf_matches(matches);
    leaf.get_one::<String>("topic")
        .map_or_else(ValueMap::new, |topic| {
            value_map([("topic", Value::String(topic.clone()))])
        })
}

pub(crate) fn completion_command() -> Command {
    Command::new("completion")
        .about("Generate or install shell completion scripts")
        .arg(Arg::new("shell").value_name("shell").num_args(0..=1))
        .arg(
            Arg::new("install")
                .long("install")
                .action(ArgAction::SetTrue)
                .help("Install completion script into shell config"),
        )
}

pub(crate) fn completion_args(matches: &ArgMatches) -> ValueMap {
    let leaf = leaf_matches(matches);
    let shell = leaf.get_one::<String>("shell").cloned();
    let install = leaf.get_flag("install");
    let mut map = value_map([("install", Value::Bool(install))]);
    if let Some(s) = shell {
        map.insert("shell".to_owned(), Value::String(s));
    }
    map
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn completion_command_parses_shell() {
        let m = completion_command()
            .try_get_matches_from(["completion", "zsh"])
            .unwrap();
        let leaf = leaf_matches(&m);
        assert_eq!(
            leaf.get_one::<String>("shell").map(String::as_str),
            Some("zsh")
        );
    }

    #[test]
    fn completion_command_parses_install() {
        let m = completion_command()
            .try_get_matches_from(["completion", "--install"])
            .unwrap();
        let leaf = leaf_matches(&m);
        assert!(leaf.get_flag("install"));
    }

    #[test]
    fn completion_command_rejects_unknown_flag() {
        assert!(
            completion_command()
                .try_get_matches_from(["completion", "--bogusflag"])
                .is_err()
        );
    }
}
