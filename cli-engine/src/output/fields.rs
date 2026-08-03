use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// Parsed field-selection tree.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FieldTree {
    children: BTreeMap<String, Option<Box<FieldTree>>>,
}

/// Parses comma-separated field paths.
#[must_use]
pub fn parse_fields(fields: &str) -> FieldTree {
    let mut root = FieldTree::default();
    for part in fields
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        insert_path(&mut root, part);
    }
    root
}

/// Applies field projection to objects or arrays of objects.
#[must_use]
pub fn filter_fields(data: &Value, fields: &str) -> Value {
    let fields = fields.trim();
    if fields.is_empty() || fields == "all" || fields == "*" {
        return data.clone();
    }
    let allowed = parse_fields(fields);
    match data {
        Value::Array(items) => {
            if items
                .iter()
                .any(|item| !matches!(item, Value::Object(_) | Value::Null))
            {
                return data.clone();
            }
            Value::Array(
                items
                    .iter()
                    .map(|item| match item {
                        Value::Object(map) => Value::Object(filter_map(map, &allowed)),
                        other => other.clone(),
                    })
                    .collect(),
            )
        }
        Value::Object(map) => Value::Object(filter_map(map, &allowed)),
        other => other.clone(),
    }
}

fn insert_path(tree: &mut FieldTree, path: &str) {
    let Some((top, rest)) = path.split_once('.') else {
        tree.children.insert(path.to_owned(), None);
        return;
    };
    if matches!(tree.children.get(top), Some(None)) {
        return;
    }
    let subtree = tree
        .children
        .entry(top.to_owned())
        .or_insert_with(|| Some(Box::<FieldTree>::default()));
    if let Some(subtree) = subtree {
        insert_path(subtree, rest);
    }
}

fn filter_map(map: &Map<String, Value>, allowed: &FieldTree) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, subtree) in &allowed.children {
        let Some(value) = map.get(key) else {
            continue;
        };
        let filtered = match subtree {
            None => value.clone(),
            Some(subtree) => filter_nested(value, subtree),
        };
        out.insert(key.clone(), filtered);
    }
    out
}

fn filter_nested(value: &Value, subtree: &FieldTree) -> Value {
    match value {
        Value::Object(map) => Value::Object(filter_map(map, subtree)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| match item {
                    Value::Object(map) => Value::Object(filter_map(map, subtree)),
                    other => other.clone(),
                })
                .collect(),
        ),
        other => other.clone(),
    }
}
