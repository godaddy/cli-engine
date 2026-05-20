use clap::{Arg, ArgMatches, Command};
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
