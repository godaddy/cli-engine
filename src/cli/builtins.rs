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

pub(crate) fn search_command() -> Command {
    Command::new("search")
        .about("Search commands and guides by keyword")
        .long_about("Searches command names, descriptions, aliases, and guide content for the given keyword(s). Narrow results to part of the command tree with --scope, e.g. --scope domain or --scope domain:list.")
        .arg(
            Arg::new("query")
                .value_name("QUERY")
                .num_args(1..)
                .required(true)
                .help("Keyword(s) to search for"),
        )
        .arg(
            Arg::new("scope")
                .long("scope")
                .value_name("PATH")
                .help("Limit results to one command subtree, e.g. domain or domain:list"),
        )
}

pub(crate) fn search_args(matches: &ArgMatches) -> ValueMap {
    let leaf = leaf_matches(matches);
    let query = leaf
        .get_many::<String>("query")
        .map(|values| values.map(String::as_str).collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let mut map = value_map([("query", Value::String(query))]);
    if let Some(scope) = leaf.get_one::<String>("scope") {
        map.insert("scope".to_owned(), Value::String(scope.clone()));
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

    #[test]
    fn search_args_joins_multi_word_query_with_spaces() {
        let m = search_command()
            .try_get_matches_from(["search", "deploy", "pipeline"])
            .unwrap();
        assert_eq!(
            search_args(&m).get("query"),
            Some(&Value::String("deploy pipeline".to_owned()))
        );
    }

    #[test]
    fn search_args_leaves_a_single_quoted_token_unchanged() {
        let m = search_command()
            .try_get_matches_from(["search", "deploy pipeline"])
            .unwrap();
        assert_eq!(
            search_args(&m).get("query"),
            Some(&Value::String("deploy pipeline".to_owned()))
        );
    }

    #[test]
    fn search_args_parses_scope() {
        let m = search_command()
            .try_get_matches_from(["search", "foo", "--scope", "domain:list"])
            .unwrap();
        let args = search_args(&m);
        assert_eq!(args.get("query"), Some(&Value::String("foo".to_owned())));
        assert_eq!(
            args.get("scope"),
            Some(&Value::String("domain:list".to_owned()))
        );
    }

    #[test]
    fn search_args_omits_scope_when_absent() {
        let m = search_command()
            .try_get_matches_from(["search", "foo"])
            .unwrap();
        assert_eq!(search_args(&m).get("scope"), None);
    }

    #[test]
    fn search_command_requires_a_query() {
        assert!(search_command().try_get_matches_from(["search"]).is_err());
    }
}
