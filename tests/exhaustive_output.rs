use std::{sync::Arc, thread};

use cli_engine::{
    Envelope, FieldInfo, HumanViewDef, OutputFormat, PaginationMeta, PipelineOpts, SchemaInfo,
    TableColumn, TreeNode, apply_pipeline, filter_fields, global_human_view_registry_snapshot,
    global_schema_registry_snapshot, is_valid_output_format, register_global_human_view,
    register_global_schema_info, render, render_format, render_human_with_view,
};
use serde_json::{Value, json};

#[test]
fn field_projection_exhaustive_known_shapes() {
    let data = json!({
        "id": "p1",
        "name": "portal",
        "owner": {
            "name": "Ada",
            "email": "ada@example.test",
            "team": {"id": "team-1", "name": "Platform"}
        },
        "tags": [
            {"key": "env", "value": "prod", "secret": "no"},
            {"key": "tier", "value": "one", "secret": "no"}
        ],
        "enabled": true
    });

    let cases = [
        ("", data.clone()),
        ("all", data.clone()),
        ("*", data.clone()),
        ("id", json!({"id": "p1"})),
        ("missing", json!({})),
        (" id , name ", json!({"id": "p1", "name": "portal"})),
        ("owner.name", json!({"owner": {"name": "Ada"}})),
        (
            "owner.team.name",
            json!({"owner": {"team": {"name": "Platform"}}}),
        ),
        (
            "tags.key,tags.value",
            json!({"tags": [{"key": "env", "value": "prod"}, {"key": "tier", "value": "one"}]}),
        ),
        ("owner,owner.name", json!({"owner": data["owner"].clone()})),
        (
            "owner.name,owner",
            json!({"owner": {"name": "Ada", "email": "ada@example.test", "team": {"id": "team-1", "name": "Platform"}}}),
        ),
    ];

    for (fields, expected) in cases {
        assert_eq!(filter_fields(&data, fields), expected, "fields={fields:?}");
    }
}

#[test]
fn field_projection_preserves_non_projectable_inputs() {
    let mixed = json!([{"id": 1}, true, {"id": 2}]);
    assert_eq!(filter_fields(&mixed, "id"), mixed);

    for scalar in [Value::Null, json!(true), json!(42), json!("name")] {
        assert_eq!(filter_fields(&scalar, "id"), scalar);
    }
}

#[test]
fn pagination_matrix_covers_offsets_limits_and_errors() {
    for len in 0_usize..=6 {
        for offset in -2_i64..=8 {
            for limit in -1_i64..=8 {
                let mut data = Value::Array((0..len).map(|item| json!({"n": item})).collect());
                let result = apply_pipeline(
                    &mut data,
                    &PipelineOpts {
                        offset,
                        limit,
                        ..PipelineOpts::default()
                    },
                );

                if limit <= 0 && offset <= 0 {
                    assert!(result.expect("no pagination").is_none());
                    assert_eq!(data.as_array().expect("array").len(), len);
                    continue;
                }

                if offset < 0 {
                    assert!(
                        result
                            .expect_err("negative offset with active pagination should fail")
                            .to_string()
                            .contains("offset must be non-negative")
                    );
                    continue;
                }

                let pagination = result
                    .expect("pagination should succeed")
                    .expect("metadata");
                let start = usize::try_from(offset).expect("non-negative").min(len);
                let end = if limit > 0 {
                    start
                        .saturating_add(usize::try_from(limit).expect("positive"))
                        .min(len)
                } else {
                    len
                };
                assert_eq!(
                    pagination,
                    PaginationMeta {
                        total: i64::try_from(len).expect("small len"),
                        offset,
                        limit,
                        count: i64::try_from(end - start).expect("small count"),
                    }
                );
                let actual = data
                    .as_array()
                    .expect("array")
                    .iter()
                    .map(|item| item["n"].as_u64().expect("n") as usize)
                    .collect::<Vec<_>>();
                assert_eq!(actual, (start..end).collect::<Vec<_>>());
            }
        }
    }
}

#[test]
fn pipeline_reports_malformed_filter_and_expr_inputs() {
    let mut object = json!({"id": 1});
    let filter_error = apply_pipeline(
        &mut object,
        &PipelineOpts {
            filter: "id == `1`".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect_err("filter on non-list data should fail");
    assert_eq!(
        filter_error.to_string(),
        "filter requires list data; use --expr for single objects"
    );

    let mut list = json!([{"id": 1}]);
    let query_error = apply_pipeline(
        &mut list,
        &PipelineOpts {
            expr: "[?".to_owned(),
            ..PipelineOpts::default()
        },
    )
    .expect_err("invalid JMESPath should fail");
    assert!(query_error.to_string().contains("invalid JMESPath query"));
}

#[test]
fn renderer_format_matrix_has_stable_success_and_error_shapes() {
    assert!(is_valid_output_format("json"));
    assert!(is_valid_output_format("human"));
    assert!(is_valid_output_format("toon"));
    for invalid in ["", "yaml", "JSON", "table"] {
        assert!(!is_valid_output_format(invalid));
    }

    let mut success = Envelope::success(json!({"name": "alpha", "enabled": true}), "things");
    success.metadata = None;
    assert_eq!(
        render(OutputFormat::Json, &success).expect("json"),
        "{\n  \"data\": {\n    \"enabled\": true,\n    \"name\": \"alpha\"\n  }\n}\n"
    );
    assert_eq!(
        render(OutputFormat::Human, &success).expect("human"),
        "enabled: yes\nname: alpha\n"
    );
    assert_eq!(
        render(OutputFormat::Toon, &success).expect("toon"),
        "data:\n  enabled: true\n  name: alpha"
    );

    let mut error = Envelope::error("BAD_REQUEST", "bad request", "things");
    error.metadata = None;
    assert_eq!(
        render_format("json", &error).expect("json error"),
        "{\n  \"error\": {\n    \"code\": \"BAD_REQUEST\",\n    \"message\": \"bad request\",\n    \"system\": \"things\"\n  }\n}\n"
    );
    assert_eq!(
        render_format("human", &error).expect("human error"),
        "Error: bad request\n"
    );
}

#[test]
fn human_view_columns_preserve_shape_for_empty_missing_and_nested_values() {
    let columns = vec![
        TableColumn::new("id", "ID"),
        TableColumn::new("owner.name", "Owner"),
        TableColumn::new("missing", "Missing"),
    ];
    let envelope = Envelope::success(
        json!([
            {"id": "p1", "owner": {"name": "Ada"}},
            {"id": "p2", "owner": {}}
        ]),
        "project:list",
    );

    assert_eq!(
        render_human_with_view(&envelope, Some(&columns)),
        "ID  OWNER  MISSING\n--  -----  -------\np1                \np2                \n\n(2 rows)\n"
    );
}

#[test]
fn human_view_no_truncate_column_preserves_long_values_in_table_output() {
    let long_url = "https://certs.godaddy.com/repository/registration-agreement.pdf";
    assert!(long_url.len() > 40, "fixture must exceed the default cap");
    let columns = vec![
        TableColumn::new("title", "Title"),
        TableColumn::new("url", "URL").no_truncate(true),
    ];
    let envelope = Envelope::success(
        json!([{"title": "Registration Agreement", "url": long_url}]),
        "agreements:list",
    );

    let rendered = render_human_with_view(&envelope, Some(&columns));

    assert!(
        rendered.contains(long_url),
        "no_truncate column must render the full URL untruncated: {rendered}"
    );
    assert!(
        !rendered.contains("..."),
        "no column in this fixture should be truncated: {rendered}"
    );
}

#[test]
fn global_registries_tolerate_repeated_and_concurrent_registration() {
    let prefix = format!(
        "concurrent:{}:{:?}",
        std::process::id(),
        thread::current().id()
    );
    let prefix = Arc::new(prefix);
    let mut handles = Vec::new();

    for index in 0..16 {
        let prefix = Arc::clone(&prefix);
        handles.push(thread::spawn(move || {
            let id = format!("{prefix}:command:{index}");
            register_global_schema_info(
                id.clone(),
                SchemaInfo::new(id.clone()).with_fields(vec![FieldInfo::new("id", "string")]),
            );
            register_global_human_view(HumanViewDef::new(id, vec![TableColumn::new("id", "ID")]));
        }));
    }

    for handle in handles {
        handle.join().expect("registry thread should not panic");
    }

    let schemas = global_schema_registry_snapshot();
    let views = global_human_view_registry_snapshot();
    for index in 0..16 {
        let id = format!("{prefix}:command:{index}");
        assert_eq!(
            schemas.get_by_path(&id).expect("schema").fields,
            vec![FieldInfo::new("id", "string")]
        );
        assert_eq!(
            views.columns(&id).expect("columns"),
            &[TableColumn::new("id", "ID")]
        );
    }
}

#[test]
fn tree_builder_preserves_explicit_hierarchy() {
    let tree =
        TreeNode::new("my-cli", "Team CLI", "my-cli").with_child(
            TreeNode::new("project", "Manage projects", "my-cli project").with_child(
                TreeNode::new("list", "List projects", "my-cli project list"),
            ),
        );

    assert_eq!(tree.children[0].children[0].path, "my-cli project list");
    assert_eq!(
        cli_engine::render_tree_human(&tree),
        "my-cli\n└── project ··· Manage projects\n    └── list ··· List projects\n"
    );
}
