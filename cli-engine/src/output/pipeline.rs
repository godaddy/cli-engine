use std::collections::BTreeSet;

use serde_json::Value;

use crate::{CliCoreError, Result};

use super::{PaginationMeta, filter_fields, parse_fields};

/// Options for the output pipeline.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PipelineOpts {
    /// JMESPath predicate applied to each list item.
    pub filter: String,
    /// Client-side page size.
    pub limit: i64,
    /// Client-side page offset.
    pub offset: i64,
    /// JMESPath expression applied to the whole result.
    pub expr: String,
    /// Comma-separated field projection.
    pub fields: String,
}

/// Applies filter, pagination, expression, and field projection in framework order.
pub fn apply_pipeline(data: &mut Value, opts: &PipelineOpts) -> Result<Option<PaginationMeta>> {
    if !opts.filter.is_empty() {
        apply_filter(data, &opts.filter)?;
    }
    let pagination = if opts.limit > 0 || opts.offset > 0 {
        apply_pagination(data, opts.offset, opts.limit)?
    } else {
        None
    };
    if !opts.expr.is_empty() {
        apply_expr(data, &opts.expr)?;
    }
    if !opts.fields.is_empty() {
        validate_fields(data, &opts.fields)?;
        *data = filter_fields(data, &opts.fields);
    }
    Ok(pagination)
}

/// Rejects `--fields` names absent from the response data, instead of
/// silently projecting them into empty rows. Skipped when the data's shape
/// gives no signal about which fields exist — an empty list, a list of
/// non-object items, or a scalar — since [`filter_fields`] passes those
/// through untouched too.
fn validate_fields(data: &Value, fields: &str) -> Result<()> {
    let fields = fields.trim();
    if fields.is_empty() || fields == "all" || fields == "*" {
        return Ok(());
    }
    let Some(known) = known_top_level_keys(data) else {
        return Ok(());
    };
    let requested = parse_fields(fields);
    let unknown: Vec<&str> = requested
        .top_level_names()
        .filter(|name| !known.contains(*name))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(CliCoreError::message(unknown_fields_message(
        &unknown, &known,
    )))
}

fn known_top_level_keys(data: &Value) -> Option<BTreeSet<String>> {
    match data {
        Value::Object(map) => Some(map.keys().cloned().collect()),
        Value::Array(items) => {
            let mut keys = BTreeSet::new();
            let mut saw_object = false;
            for item in items {
                match item {
                    Value::Object(map) => {
                        saw_object = true;
                        keys.extend(map.keys().cloned());
                    }
                    Value::Null => {}
                    _ => return None,
                }
            }
            saw_object.then_some(keys)
        }
        _ => None,
    }
}

fn unknown_fields_message(unknown: &[&str], known: &BTreeSet<String>) -> String {
    let plural = if unknown.len() > 1 { "s" } else { "" };
    let quoted = unknown
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let mut message = format!("fields: unknown field{plural} {quoted}");
    if let Some(first) = unknown.first()
        && let Some(suggestion) = nearest_field(first, known)
    {
        message.push_str(&format!(" (did you mean \"{suggestion}\"?)"));
    }
    if !known.is_empty() {
        let valid = known.iter().cloned().collect::<Vec<_>>().join(", ");
        message.push_str(&format!("; valid fields: {valid}"));
    }
    message
}

/// Finds the closest known field name within edit-distance `max(1, name_len /
/// 3)`, mirroring `nearest_subcommand`'s tolerance in `cli.rs`. Ties break
/// alphabetically.
fn nearest_field(name: &str, known: &BTreeSet<String>) -> Option<String> {
    let name = name.to_ascii_lowercase();
    let max_distance = 1.max(name.chars().count() / 3);
    known
        .iter()
        .map(|candidate| {
            (
                strsim::osa_distance(&name, &candidate.to_ascii_lowercase()),
                candidate,
            )
        })
        .filter(|(distance, _)| *distance <= max_distance)
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)))
        .map(|(_, candidate)| candidate.clone())
}

fn apply_pagination(data: &mut Value, offset: i64, limit: i64) -> Result<Option<PaginationMeta>> {
    let Value::Array(items) = data else {
        return Ok(None);
    };
    let total = items.len();
    let total_i64 = match i64::try_from(total) {
        Ok(total) => total,
        Err(_) => {
            return Err(CliCoreError::message(
                "pagination: list length exceeds supported range",
            ));
        }
    };
    let start = offset.min(total_i64);
    let start = match usize::try_from(start) {
        Ok(start) => start,
        Err(_) => {
            return Err(CliCoreError::message(
                "pagination: offset must be non-negative",
            ));
        }
    };
    let mut end = total;
    if limit > 0 {
        let limit = match usize::try_from(limit) {
            Ok(limit) => limit,
            Err(_) => {
                return Err(CliCoreError::message(
                    "pagination: limit exceeds supported range",
                ));
            }
        };
        if start + limit < end {
            end = start + limit;
        }
    }
    let sliced = items[start..end].to_vec();
    *items = sliced;
    Ok(Some(PaginationMeta {
        total: total_i64,
        offset,
        limit,
        count: match i64::try_from(end - start) {
            Ok(count) => count,
            Err(_) => {
                return Err(CliCoreError::message(
                    "pagination: count exceeds supported range",
                ));
            }
        },
        has_more: end < total,
    }))
}

fn apply_filter(data: &mut Value, expression: &str) -> Result<()> {
    let Value::Array(items) = data else {
        return Err(CliCoreError::message(
            "filter requires list data; use --expr for single objects",
        ));
    };

    let expression = compile_query(expression)?;
    let mut retained = Vec::with_capacity(items.len());
    for item in items.drain(..) {
        if search_query(&expression, &item)?.is_truthy() {
            retained.push(item);
        }
    }
    *items = retained;
    Ok(())
}

fn apply_expr(data: &mut Value, expression: &str) -> Result<()> {
    let expression = compile_query(expression)?;
    let result = search_query(&expression, data)?;
    *data = serde_json::to_value(result.as_ref())
        .map_err(|error| CliCoreError::message(format!("expr: invalid result: {error}")))?;
    Ok(())
}

fn compile_query(expression: &str) -> Result<jmespath::Expression<'static>> {
    jmespath::compile(expression.trim())
        .map_err(|error| CliCoreError::message(format!("expr: invalid JMESPath query: {error}")))
}

fn search_query(expression: &jmespath::Expression<'_>, data: &Value) -> Result<jmespath::Rcvar> {
    expression
        .search(data)
        .map_err(|error| CliCoreError::message(format!("expr: JMESPath query failed: {error}")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{apply_expr, apply_pagination, compile_query, search_query, validate_fields};

    #[test]
    fn private_pipeline_helpers_cover_boundary_paths_directly() {
        let mut object = json!({"id": "p1"});
        assert_eq!(
            apply_pagination(&mut object, 10, 1).expect("object pagination should no-op"),
            None
        );
        assert_eq!(object, json!({"id": "p1"}));

        let mut items = json!([{"id": "p1"}, {"id": "p2"}]);
        let err =
            apply_pagination(&mut items, -1, 1).expect_err("negative offset should be rejected");
        assert_eq!(err.to_string(), "pagination: offset must be non-negative");

        let expression = compile_query("items[?enabled].id").expect("query should compile");
        let result = search_query(
            &expression,
            &json!({"items": [{"id": "p1", "enabled": true}, {"id": "p2", "enabled": false}]}),
        )
        .expect("query should evaluate");
        assert_eq!(
            serde_json::to_value(result.as_ref()).expect("result should serialize"),
            json!(["p1"])
        );

        let mut data = json!({"items": [{"id": "p1"}]});
        apply_expr(&mut data, "items[0].id").expect("expr should replace data");
        assert_eq!(data, json!("p1"));
    }

    #[test]
    fn validate_fields_rejects_a_name_absent_from_every_row_with_a_helpful_message() {
        let data = json!([
            {"operationId": "search", "description": "Search the catalog"},
            {"operationId": "get", "description": "Get an item"},
        ]);

        let err = validate_fields(&data, "OPERATIONID,description")
            .expect_err("uppercase name is not a real key");
        let message = err.to_string();
        assert!(
            message.contains("unknown field \"OPERATIONID\""),
            "{message}"
        );
        assert!(
            message.contains("did you mean \"operationId\"?"),
            "{message}"
        );
        assert!(
            message.contains("valid fields: description, operationId"),
            "{message}"
        );
    }

    #[test]
    fn validate_fields_accepts_names_present_on_at_least_one_row() {
        let data = json!([{"id": "p1", "extra": true}, {"id": "p2"}]);
        validate_fields(&data, "id,extra").expect("both names appear on some row");
    }

    #[test]
    fn validate_fields_only_checks_the_top_level_segment_of_nested_paths() {
        let data = json!({"content": {"text": "hi"}});
        validate_fields(&data, "content.text").expect("nested path's top segment is known");
    }

    #[test]
    fn validate_fields_skips_shapes_that_give_no_signal() {
        let empty_list = json!([]);
        validate_fields(&empty_list, "anything").expect("empty list can't validate");

        let scalar_list = json!(["a", "b"]);
        validate_fields(&scalar_list, "anything").expect("non-object items can't validate");

        let scalar = json!("just a string");
        validate_fields(&scalar, "anything").expect("scalar data can't validate");
    }

    #[test]
    fn validate_fields_passes_through_all_and_star_and_empty() {
        let data = json!({"id": "p1"});
        validate_fields(&data, "all").expect("all bypasses validation");
        validate_fields(&data, "*").expect("star bypasses validation");
        validate_fields(&data, "").expect("empty bypasses validation");
    }
}
