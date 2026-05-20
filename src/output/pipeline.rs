use serde_json::Value;

use crate::{CliCoreError, Result};

use super::{PaginationMeta, filter_fields};

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
        *data = filter_fields(data, &opts.fields);
    }
    Ok(pagination)
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

    use super::{apply_expr, apply_pagination, compile_query, search_query};

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
}
