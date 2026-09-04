use clap::{Arg, ArgAction, ArgMatches};
use serde_json::{Number, Value};

use super::CommandSpec;
use crate::middleware::ValueMap;

/// Extracts the colon-separated command path from parsed `clap` matches.
#[must_use]
pub fn command_path_from_matches(root_name: &str, matches: &ArgMatches) -> String {
    let mut parts = Vec::new();
    let mut current = matches;
    while let Some((name, submatches)) = current.subcommand() {
        if name != root_name {
            parts.push(name.to_owned());
        }
        current = submatches;
    }
    parts.join(":")
}

/// Builds a colon-separated command path from path parts.
///
/// The optional annotation is used only for isolated single-command tests.
#[must_use]
pub fn command_path_from_parts(parts: &[impl AsRef<str>], path_annotation: Option<&str>) -> String {
    if parts.is_empty() {
        return String::new();
    }
    if parts.len() > 1 {
        return parts[1..]
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>()
            .join(":");
    }
    path_annotation
        .filter(|annotation| !annotation.is_empty())
        .map_or_else(|| parts[0].as_ref().to_owned(), ToOwned::to_owned)
}

/// Returns the deepest subcommand matches.
#[must_use]
pub fn leaf_matches(matches: &ArgMatches) -> &ArgMatches {
    let mut current = matches;
    while let Some((_, submatches)) = current.subcommand() {
        current = submatches;
    }
    current
}

/// Converts parsed command arguments into the JSON-ish map consumed by middleware.
///
/// When `changed_only` is true, only arguments that came from the command line
/// are included. This is the user-args map used by authz and audit.
#[must_use]
pub fn command_args_from_matches(
    matches: &ArgMatches,
    spec: &CommandSpec,
    changed_only: bool,
) -> ValueMap {
    let mut args = ValueMap::new();
    for arg in &spec.args {
        let id = arg.get_id().to_string();
        let changed = matches
            .value_source(&id)
            .is_some_and(|source| source == clap::parser::ValueSource::CommandLine);
        if changed_only && !changed {
            continue;
        }
        if let Some(value) = arg_value_from_matches(matches, arg, &id) {
            args.insert(id, value);
        }
    }
    args
}

fn arg_value_from_matches(matches: &ArgMatches, flag: &Arg, id: &str) -> Option<Value> {
    matches.value_source(id)?;

    if matches!(flag.get_action(), ArgAction::SetTrue | ArgAction::SetFalse)
        && let Some(value) = matches.get_one::<bool>(id)
    {
        return Some(Value::Bool(*value));
    }

    if let Some(value) = typed_arg_value_from_matches(matches, id) {
        return Some(value);
    }

    if let Some(values) = matches.get_raw(id) {
        let rendered = values
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        return match rendered.as_slice() {
            [] => None,
            [single] => Some(Value::String(single.clone())),
            _ => Some(Value::Array(
                rendered.into_iter().map(Value::String).collect(),
            )),
        };
    }

    if let Some(value) = matches.get_one::<String>(id) {
        return Some(Value::String(value.clone()));
    }
    if let Some(value) = matches.get_one::<usize>(id) {
        return Some(serde_json::json!(value));
    }
    if let Some(value) = matches.get_one::<u64>(id) {
        return Some(serde_json::json!(value));
    }
    if let Some(value) = matches.get_one::<i64>(id) {
        return Some(serde_json::json!(value));
    }
    None
}

fn typed_arg_value_from_matches(matches: &ArgMatches, id: &str) -> Option<Value> {
    typed_values::<bool>(matches, id, Value::Bool)
        .or_else(|| typed_values::<i8>(matches, id, |value| Value::Number(value.into())))
        .or_else(|| typed_values::<i16>(matches, id, |value| Value::Number(value.into())))
        .or_else(|| typed_values::<i64>(matches, id, |value| Value::Number(value.into())))
        .or_else(|| typed_values::<i32>(matches, id, |value| Value::Number(value.into())))
        .or_else(|| typed_values::<u8>(matches, id, |value| Value::Number(value.into())))
        .or_else(|| typed_values::<u16>(matches, id, |value| Value::Number(value.into())))
        .or_else(|| typed_values::<u64>(matches, id, |value| Value::Number(value.into())))
        .or_else(|| typed_values::<u32>(matches, id, |value| Value::Number(value.into())))
        .or_else(|| {
            typed_values::<usize>(matches, id, |value| {
                u64::try_from(value).map_or(Value::Null, |value| Value::Number(value.into()))
            })
        })
        .or_else(|| {
            typed_values::<f64>(matches, id, |value| {
                Number::from_f64(value).map_or(Value::Null, Value::Number)
            })
        })
        .or_else(|| {
            typed_values::<f32>(matches, id, |value| {
                Number::from_f64(f64::from(value)).map_or(Value::Null, Value::Number)
            })
        })
        .or_else(|| typed_values::<String>(matches, id, Value::String))
}

fn typed_values<T>(matches: &ArgMatches, id: &str, to_value: impl Fn(T) -> Value) -> Option<Value>
where
    T: Clone + Send + Sync + 'static,
{
    let Ok(Some(values)) = matches.try_get_many::<T>(id) else {
        return None;
    };
    let values = values.cloned().map(to_value).collect::<Vec<_>>();
    match values.as_slice() {
        [] => None,
        [single] => Some(single.clone()),
        _ => Some(Value::Array(values)),
    }
}
