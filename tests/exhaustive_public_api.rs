use std::{borrow::Cow, collections::BTreeMap};

use clap::{Arg, ArgAction, Command};
use cli_engine::{
    CliCoreError, CommandSpec, DetailedError, FieldInfo, GlobalFlags, OutputField, OutputFormat,
    SchemaInfo, command_args_from_matches, command_path_from_matches, command_path_from_parts,
    derive_bool_flags, derive_value_flags, exit_code_for_error, exit_code_for_exit_coder,
    extract_command_path, fields_from_json_schema, global_flags_from_matches, has_true_schema_flag,
    json_schema_info, register_global_flags,
};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};

fn command_with_common_args() -> Command {
    register_global_flags(
        Command::new("my-cli").subcommand(
            Command::new("project")
                .alias("p")
                .subcommand(
                    Command::new("list")
                        .alias("ls")
                        .arg(Arg::new("team").long("team").short('t'))
                        .arg(Arg::new("active").long("active").action(ArgAction::SetTrue))
                        .arg(Arg::new("names").long("name").num_args(1..)),
                )
                .subcommand(
                    Command::new("delete").arg(
                        Arg::new("id")
                            .long("id")
                            .required(true)
                            .allow_hyphen_values(true),
                    ),
                ),
        ),
    )
}

#[test]
fn raw_command_path_extraction_covers_flags_values_aliases_and_negatives() {
    let command = command_with_common_args();
    let bool_flags = derive_bool_flags(&command);
    let value_flags = derive_value_flags(&command);

    assert!(bool_flags.contains("--schema"));
    assert!(bool_flags.contains("--dry-run"));
    assert!(bool_flags.contains("--active"));
    assert!(value_flags.contains("--team"));
    assert!(value_flags.contains("-t"));
    assert!(value_flags.contains("--id"));

    let cases = [
        (
            vec!["my-cli", "project", "list"],
            "project:list",
            "simple nested path",
        ),
        (
            vec!["my-cli", "p", "ls", "--team", "platform", "--active"],
            "p:ls",
            "aliases are preserved in raw extraction",
        ),
        (
            vec!["my-cli", "--output", "human", "project", "list"],
            "project:list",
            "global value flags before commands are skipped",
        ),
        (
            vec![
                "my-cli",
                "project",
                "list",
                "--team=platform",
                "--dry-run=true",
                "--schema=false",
            ],
            "project:list",
            "equals-form flags are skipped",
        ),
        (
            vec!["my-cli", "project", "delete", "--id", "-123"],
            "project:delete",
            "hyphenated values for known value flags are skipped",
        ),
        (
            vec!["my-cli", "project", "--unknown", "value", "list"],
            "project:list",
            "unknown flag followed by a value consumes that value",
        ),
        (
            vec!["my-cli", "--schema", "project", "list"],
            "project:list",
            "bare schema flag does not consume command tokens",
        ),
    ];

    for (args, expected, label) in cases {
        assert_eq!(
            extract_command_path(&args, &bool_flags, &value_flags),
            expected,
            "{label}: {args:?}"
        );
    }
}

#[test]
fn parsed_global_flags_cover_defaults_short_aliases_and_optional_values() {
    let matches = command_with_common_args()
        .try_get_matches_from([
            "my-cli",
            "-o",
            "toon",
            "--verbose",
            "--debug",
            "transport",
            "--dry-run=true",
            "--schema=false",
            "--fields",
            "id,name",
            "--filter",
            "active == `true`",
            "--expr",
            "[].id",
            "--limit",
            "10",
            "--offset",
            "2",
            "--reason",
            "cleanup",
            "--timeout",
            "5m",
            "--search",
            "projects",
            "project",
            "list",
            "--team",
            "platform",
            "--active",
        ])
        .expect("global flags should parse");

    assert_eq!(
        command_path_from_matches("my-cli", &matches),
        "project:list"
    );
    assert_eq!(
        global_flags_from_matches(&matches, "json"),
        GlobalFlags {
            output_format: "toon".to_owned(),
            verbose: "all".to_owned(),
            dry_run: true,
            fields: "id,name".to_owned(),
            filter: "active == `true`".to_owned(),
            expr: "[].id".to_owned(),
            limit: 10,
            offset: 2,
            schema: false,
            reason: "cleanup".to_owned(),
            timeout: "5m".to_owned(),
            debug: "transport".to_owned(),
            search: "projects".to_owned(),
        }
    );
}

#[test]
fn schema_flag_truth_table_matches_documented_boolean_surface() {
    for value in ["1", "t", "T", "TRUE", "true", "True"] {
        assert!(
            has_true_schema_flag(&["my-cli", &format!("--schema={value}")]),
            "{value}"
        );
    }

    for value in ["0", "f", "F", "FALSE", "false", "False", "maybe", ""] {
        assert!(
            !has_true_schema_flag(&["my-cli", &format!("--schema={value}")]),
            "{value}"
        );
    }

    assert!(has_true_schema_flag(&["my-cli", "--schema"]));
    assert!(!has_true_schema_flag(&["my-cli", "project", "list"]));
}

#[test]
fn command_args_from_matches_preserves_public_value_shapes() {
    let spec = CommandSpec::new("list", "List projects")
        .with_arg(Arg::new("team").long("team").default_value("all"))
        .with_arg(Arg::new("active").long("active").action(ArgAction::SetTrue))
        .with_arg(
            Arg::new("name")
                .long("name")
                .num_args(1..)
                .action(ArgAction::Append),
        );
    let command = spec.clap_command();
    let matches = command
        .try_get_matches_from(["list", "--active", "--name", "api", "--name", "web"])
        .expect("command args should parse");

    let all_args = command_args_from_matches(&matches, &spec, false);
    assert_eq!(all_args["team"], Value::String("all".to_owned()));
    assert_eq!(all_args["active"], Value::Bool(true));
    assert_eq!(
        all_args["name"],
        json!(["api", "web"]),
        "repeated values remain arrays"
    );

    let changed_args = command_args_from_matches(&matches, &spec, true);
    assert!(!changed_args.contains_key("team"));
    assert_eq!(changed_args["active"], Value::Bool(true));
    assert_eq!(changed_args["name"], json!(["api", "web"]));
}

#[derive(Serialize, JsonSchema)]
struct SchemaMatrix {
    name: String,
    count: i64,
    ratio: f64,
    active: bool,
    labels: Vec<String>,
    nested: NestedSchema,
    optional: Option<String>,
    optional_numbers: Option<Vec<i64>>,
}

#[derive(Serialize, JsonSchema)]
struct NestedSchema {
    id: String,
}

#[test]
fn rust_json_schema_summary_covers_scalar_array_object_and_optional_fields() {
    let info = json_schema_info::<SchemaMatrix>("project:list");
    assert_eq!(info.command, "project:list");
    assert_eq!(
        info.schema
            .as_ref()
            .and_then(|schema| schema.get("title"))
            .and_then(Value::as_str),
        Some("SchemaMatrix")
    );

    let fields = info
        .fields
        .into_iter()
        .map(|field| (field.name, (field.field_type, field.optional)))
        .collect::<BTreeMap<_, _>>();

    assert_eq!(fields["active"], ("bool".to_owned(), false));
    assert_eq!(fields["count"], ("int".to_owned(), false));
    assert_eq!(fields["labels"], ("[]string".to_owned(), false));
    assert_eq!(fields["name"], ("string".to_owned(), false));
    assert_eq!(fields["nested"], ("object".to_owned(), false));
    assert_eq!(fields["optional"], ("string".to_owned(), true));
    assert_eq!(fields["optional_numbers"], ("[]int".to_owned(), true));
    assert_eq!(fields["ratio"], ("float".to_owned(), false));
}

#[test]
fn manual_json_schema_field_extraction_covers_schema_edge_shapes() {
    let schema = json!({
        "type": "object",
        "required": ["plain", "nullable_union", "array_without_items", "inline_object", "unknown"],
        "properties": {
            "plain": {"type": "string"},
            "nullable_union": {"type": ["null", "integer"]},
            "any_of_number": {"anyOf": [{"type": "null"}, {"type": "number"}]},
            "one_of_bool": {"oneOf": [{"type": "boolean"}, {"type": "null"}]},
            "array_without_items": {"type": "array"},
            "array_of_objects": {"type": "array", "items": {"type": "object"}},
            "inline_object": {"properties": {"id": {"type": "string"}}},
            "unknown": {}
        }
    });

    let fields = fields_from_json_schema(&schema)
        .into_iter()
        .map(|field| (field.name, (field.field_type, field.optional)))
        .collect::<BTreeMap<_, _>>();

    assert_eq!(fields["plain"], ("string".to_owned(), false));
    assert_eq!(fields["nullable_union"], ("int".to_owned(), true));
    assert_eq!(fields["any_of_number"], ("float".to_owned(), true));
    assert_eq!(fields["one_of_bool"], ("bool".to_owned(), true));
    assert_eq!(fields["array_without_items"], ("[]any".to_owned(), false));
    assert_eq!(fields["array_of_objects"], ("[]object".to_owned(), true));
    assert_eq!(fields["inline_object"], ("object".to_owned(), false));
    assert_eq!(fields["unknown"], ("any".to_owned(), false));
}

#[test]
fn output_field_constructors_are_exhaustive_and_schema_info_is_serializable() {
    let fields = [
        OutputField::string("name"),
        OutputField::int("count"),
        OutputField::float("ratio"),
        OutputField::bool("active"),
        OutputField::string_list("labels"),
        OutputField::int_list("ids"),
        OutputField::float_list("scores"),
        OutputField::bool_list("toggles"),
        OutputField::list("objects", "[]object").optional(),
    ];

    let actual = fields
        .into_iter()
        .map(|field| FieldInfo {
            name: field.name.to_owned(),
            field_type: field.field_type.to_owned(),
            optional: field.optional,
        })
        .collect::<Vec<FieldInfo>>();
    assert_eq!(
        actual,
        vec![
            FieldInfo::new("name", "string"),
            FieldInfo::new("count", "int"),
            FieldInfo::new("ratio", "float"),
            FieldInfo::new("active", "bool"),
            FieldInfo::new("labels", "[]string"),
            FieldInfo::new("ids", "[]int"),
            FieldInfo::new("scores", "[]float"),
            FieldInfo::new("toggles", "[]bool"),
            FieldInfo::new("objects", "[]object").optional(),
        ]
    );

    let info = SchemaInfo::new("project:list").with_fields(actual);
    assert_eq!(
        serde_json::to_value(&info).expect("schema info serializes"),
        json!({
            "command": "project:list",
            "fields": [
                {"name": "name", "type": "string", "optional": false},
                {"name": "count", "type": "int", "optional": false},
                {"name": "ratio", "type": "float", "optional": false},
                {"name": "active", "type": "bool", "optional": false},
                {"name": "labels", "type": "[]string", "optional": false},
                {"name": "ids", "type": "[]int", "optional": false},
                {"name": "scores", "type": "[]float", "optional": false},
                {"name": "toggles", "type": "[]bool", "optional": false},
                {"name": "objects", "type": "[]object", "optional": true}
            ]
        })
    );
}

#[test]
fn command_path_helpers_cover_empty_root_and_annotation_cases() {
    let empty: [&str; 0] = [];
    assert_eq!(command_path_from_parts(&empty, None), "");
    assert_eq!(command_path_from_parts(&["root"], None), "root");
    assert_eq!(
        command_path_from_parts(&["root"], Some("annotated:path")),
        "annotated:path"
    );
    assert_eq!(command_path_from_parts(&["root", "a", "b"], None), "a:b");
}

#[derive(Debug, thiserror::Error)]
#[error("coded failure")]
struct CodedFailure;

impl cli_engine::ExitCoder for CodedFailure {
    fn exit_code(&self) -> i32 {
        44
    }
}

impl DetailedError for CodedFailure {
    fn error_code(&self) -> Cow<'static, str> {
        Cow::Borrowed("CODED")
    }

    fn error_system(&self) -> Option<Cow<'static, str>> {
        Some(Cow::Borrowed("tests"))
    }

    fn error_request_id(&self) -> Option<Cow<'static, str>> {
        None
    }
}

#[test]
fn error_exit_codes_cover_framework_custom_and_generic_errors() {
    assert_eq!(exit_code_for_exit_coder(&CodedFailure), 44);
    assert_eq!(
        exit_code_for_error(&CliCoreError::message("framework failure")),
        1
    );
    assert_eq!(
        exit_code_for_error(&CliCoreError::with_exit_code(
            7,
            CliCoreError::message("not found")
        )),
        7
    );
}

#[test]
fn output_format_parse_matrix_is_complete_for_public_variants() {
    let variants = [
        ("json", OutputFormat::Json),
        ("human", OutputFormat::Human),
        ("toon", OutputFormat::Toon),
    ];

    for (raw, expected) in variants {
        assert_eq!(
            raw.parse::<OutputFormat>()
                .expect("known output format parses"),
            expected
        );
    }

    for raw in ["", "JSON", "yaml", "table", "text"] {
        assert_eq!(
            raw.parse::<OutputFormat>()
                .expect("unknown output format defaults to json"),
            OutputFormat::Json,
            "{raw}"
        );
    }
}
