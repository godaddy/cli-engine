use serde_json::{Value, json};

use crate::output::PaginationMeta;
use crate::output::human::value_format::{
    resolve_field_parent, resolve_field_path, resolve_nested_pagination,
};

#[test]
fn resolve_field_path_walks_dotted_wrapper_and_reports_missing_or_wrong_shape() {
    let map = json!({
        "parameters": { "items": [{"name": "limit"}], "total": 1 },
        "owner": "not-an-object",
    });
    let map = map.as_object().expect("object fixture");

    assert_eq!(
        resolve_field_path(map, "parameters.items"),
        map.get("parameters").and_then(|value| value.get("items"))
    );
    assert_eq!(resolve_field_path(map, "parameters.missing"), None);
    assert_eq!(
        resolve_field_path(map, "owner.name"),
        None,
        "intermediate value is a string, not an object"
    );
    assert_eq!(resolve_field_path(map, "missing"), None);
    assert_eq!(resolve_field_path(map, ""), None, "empty field");
    assert_eq!(resolve_field_path(map, ".parameters"), None, "leading dot");
    assert_eq!(resolve_field_path(map, "parameters."), None, "trailing dot");
    assert_eq!(
        resolve_field_path(map, "parameters..items"),
        None,
        "doubled dot"
    );
}

#[test]
fn resolve_field_parent_returns_parent_object_for_dotted_and_bare_fields() {
    let map = json!({
        "parameters": { "items": [], "total": 2 },
        "owner": "not-an-object",
    });
    let map = map.as_object().expect("object fixture");

    assert_eq!(
        resolve_field_parent(map, "parameters.items"),
        map.get("parameters").and_then(Value::as_object)
    );
    assert_eq!(
        resolve_field_parent(map, "items"),
        Some(map),
        "a field with no dot has the object being rendered as its own parent"
    );
    assert_eq!(
        resolve_field_parent(map, "owner.name"),
        None,
        "intermediate value is a string, not an object"
    );
    assert_eq!(resolve_field_parent(map, "missing.items"), None);
}

#[test]
fn resolve_nested_pagination_deserializes_a_pagination_meta_shaped_sibling() {
    let parent = json!({
        "pagination": { "total": 26, "offset": 0, "limit": 2, "count": 2, "has_more": true },
    });
    let parent = parent.as_object().expect("object fixture");

    let meta = resolve_nested_pagination(parent).expect("pagination sibling present");
    assert_eq!(
        meta,
        PaginationMeta {
            total: 26,
            offset: 0,
            limit: 2,
            count: 2,
            has_more: true,
        }
    );
}

#[test]
fn resolve_nested_pagination_is_none_when_the_sibling_is_absent_or_malformed() {
    let no_sibling = json!({ "items": [] });
    assert_eq!(
        resolve_nested_pagination(no_sibling.as_object().expect("object fixture")),
        None,
        "no pagination field at all"
    );

    let wrong_shape = json!({ "pagination": { "total": 26 } });
    assert_eq!(
        resolve_nested_pagination(wrong_shape.as_object().expect("object fixture")),
        None,
        "missing required PaginationMeta fields fails to deserialize"
    );

    let not_an_object = json!({ "pagination": "26 total" });
    assert_eq!(
        resolve_nested_pagination(not_an_object.as_object().expect("object fixture")),
        None,
        "pagination field present but not object-shaped"
    );
}
