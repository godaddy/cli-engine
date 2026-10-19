use serde::Deserialize;

use super::*;

/// Output style.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Style {
    /// Plain.
    #[default]
    Plain,
    /// Fancy.
    Fancy,
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
struct Domain {
    /// Show names in ASCII form.
    punycode: bool,
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
struct Prefs {
    /// How many rows to show
    /// per page.
    page_size: Option<u32>,
    /// Pick a style.
    style: Style,
    /// Nickname.
    nickname: String,
    domain: Domain,
}

fn schema() -> ConfigSchema {
    ConfigSchema::of::<Prefs>()
}

#[test]
fn derives_keys_types_descriptions_and_defaults() {
    let schema = schema();
    let page = schema.find("page_size").expect("page_size");
    assert_eq!(page.kind, ConfigKind::Integer);
    assert_eq!(page.description, "How many rows to show per page.");
    let style = schema.find("style").expect("style");
    assert_eq!(style.kind, ConfigKind::String);
    assert_eq!(style.allowed, ["plain", "fancy"]);
    assert_eq!(style.default, Some(Value::from("plain")));
    let nested = schema.find("domain.punycode").expect("nested key");
    assert_eq!(nested.kind, ConfigKind::Bool);
    assert_eq!(nested.default, Some(Value::Bool(false)));
    assert_eq!(nested.description, "Show names in ASCII form.");
    assert!(schema.find("credentials.store").is_some(), "engine keys");
}

#[test]
fn set_stores_the_declared_type_not_an_inferred_one() {
    let mut file = ConfigFile::default();
    let schema = schema();
    schema.set(&mut file, "nickname", "123").expect("string");
    schema
        .set(&mut file, "domain.punycode", "true")
        .expect("bool");
    assert_eq!(file.get_json("nickname"), Some(Value::from("123")));
    assert_eq!(file.get_json("domain.punycode"), Some(Value::Bool(true)));
}

#[test]
fn set_rejects_bad_values_and_leaves_file_untouched() {
    let mut file = ConfigFile::default();
    let schema = schema();
    for (key, raw) in [
        ("domain.punycode", "banana"),
        ("page_size", "ten"),
        ("page_size", "-1"),
        ("style", "loud"),
    ] {
        assert!(schema.set(&mut file, key, raw).is_err(), "{key}={raw}");
    }
    assert_eq!(file.to_toml_string(), "");
}

#[test]
fn strict_schema_rejects_unknown_keys_with_a_suggestion() {
    let err = schema()
        .set(&mut ConfigFile::default(), "domain.punycod", "true")
        .expect_err("unknown");
    assert!(
        matches!(&err, CliCoreError::Fix { fix, .. } if fix.contains("domain.punycode")),
        "expected a did-you-mean fix, got {err:?}"
    );
}

#[test]
fn engine_only_schema_stays_permissive() {
    let mut file = ConfigFile::default();
    let schema = ConfigSchema::engine();
    schema
        .set(&mut file, "deploy.replicas", "3")
        .expect("free-form");
    assert_eq!(file.get_json("deploy.replicas"), Some(Value::from(3)));
    assert!(schema.set(&mut file, "credentials.store", "nope").is_err());
}

#[test]
fn validate_with_rejects_cross_field_violations() {
    let schema = ConfigSchema::of::<Prefs>().validate_with(|p: &Prefs| {
        if p.nickname == "root" && p.domain.punycode {
            Err("root cannot use punycode".to_owned())
        } else {
            Ok(())
        }
    });
    let mut file = ConfigFile::default();
    schema.set(&mut file, "nickname", "root").expect("ok alone");
    let err = schema
        .set(&mut file, "domain.punycode", "true")
        .expect_err("rule");
    assert!(
        err.to_string().contains("root cannot use punycode"),
        "{err}"
    );
    assert_eq!(file.get_json("domain.punycode"), None);
}

#[test]
fn unset_removes_key_and_empty_parent_table() {
    let mut file = ConfigFile::default();
    let schema = schema();
    schema
        .set(&mut file, "domain.punycode", "true")
        .expect("set");
    assert!(schema.unset(&mut file, "domain.punycode").expect("unset"));
    assert_eq!(file.to_toml_string().trim(), "");
    assert!(
        !schema
            .unset(&mut file, "domain.punycode")
            .expect("idempotent")
    );
    assert!(schema.unset(&mut file, "nope").is_err());
}

#[test]
fn entries_report_value_and_source() {
    let mut file = ConfigFile::default();
    let schema = schema();
    schema.set(&mut file, "style", "fancy").expect("set");
    let style = schema.entry(&file, "style").expect("style");
    assert_eq!(
        (style.value, style.source),
        (Value::from("fancy"), SettingSource::File)
    );
    let punycode = schema.entry(&file, "domain.punycode").expect("punycode");
    assert_eq!(
        (punycode.value, punycode.source),
        (Value::Bool(false), SettingSource::Default)
    );
    let page = schema.entry(&file, "page_size").expect("page_size");
    assert_eq!(
        (page.value, page.source),
        (Value::Null, SettingSource::Unset)
    );
}

/// Settings with a cross-field rule and a non-zero default.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
struct Range {
    min: u32,
    max: u32,
}

impl Default for Range {
    fn default() -> Self {
        Self { min: 0, max: 10 }
    }
}

fn range_schema() -> ConfigSchema {
    ConfigSchema::of::<Range>().validate_with(|r: &Range| {
        if r.min <= r.max {
            Ok(())
        } else {
            Err(format!("min {} exceeds max {}", r.min, r.max))
        }
    })
}

#[test]
fn unset_is_refused_when_the_default_would_break_a_rule() {
    let schema = range_schema();
    let mut file = ConfigFile::default();
    schema.set(&mut file, "max", "30").expect("max");
    schema.set(&mut file, "min", "20").expect("min");
    let before = file.to_toml_string();
    let err = schema
        .unset(&mut file, "max")
        .expect_err("default 10 < min 20");
    assert!(err.to_string().contains("min 20 exceeds max 10"), "{err}");
    assert_eq!(file.to_toml_string(), before, "file untouched");
}

#[test]
fn unset_is_allowed_while_the_file_is_already_invalid() {
    let schema = range_schema();
    let mut file = ConfigFile::default();
    file.set("min", "20").expect("hand edit");
    file.set("max", "5").expect("hand edit");
    // Still invalid afterwards (20 > default 10), but refusing would trap the user.
    assert!(schema.unset(&mut file, "max").expect("repair step"));
    assert_eq!(file.get_json("max"), None);
}

#[test]
fn rules_apply_to_engine_keys() {
    let schema = ConfigSchema::engine().validate_with(|e: &crate::config::EngineConfig| {
        match e.output.format.as_deref() {
            Some("toon") => Err("toon is not allowed here".to_owned()),
            _ => Ok(()),
        }
    });
    let mut file = ConfigFile::default();
    assert!(schema.set(&mut file, "output.format", "toon").is_err());
    assert_eq!(file.to_toml_string(), "");
    schema
        .set(&mut file, "output.format", "human")
        .expect("allowed");
}

#[test]
fn rules_apply_to_undeclared_keys_in_a_permissive_schema() {
    #[derive(Deserialize)]
    #[serde(default)]
    struct Loose {
        replicas: i64,
    }
    impl Default for Loose {
        fn default() -> Self {
            Self { replicas: 1 }
        }
    }
    let schema = ConfigSchema::engine().validate_with(|l: &Loose| {
        if l.replicas <= 5 {
            Ok(())
        } else {
            Err("at most 5 replicas".to_owned())
        }
    });
    let mut file = ConfigFile::default();
    assert!(schema.set(&mut file, "replicas", "9").is_err());
    assert_eq!(file.get_json("replicas"), None);
    schema
        .set(&mut file, "replicas", "3")
        .expect("within the rule");
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
struct OptionalSection {
    domain: Option<Domain>,
}

#[test]
fn child_default_applies_only_once_the_optional_section_exists() {
    let schema = ConfigSchema::of::<OptionalSection>();

    let absent = schema
        .entry(&ConfigFile::default(), "domain.punycode")
        .expect("entry");
    assert_eq!(
        absent.key.default, None,
        "no section, so no default is advertised either"
    );
    assert_eq!(
        (absent.value, absent.source),
        (Value::Null, SettingSource::Unset)
    );

    let mut file = ConfigFile::default();
    file.set("domain.other", "1").expect("creates [domain]");
    let present = schema.entry(&file, "domain.punycode").expect("entry");
    assert_eq!(present.key.default, Some(Value::Bool(false)));
    assert_eq!(
        (present.value, present.source),
        (Value::Bool(false), SettingSource::Default)
    );

    let mut file = ConfigFile::default();
    schema
        .set(&mut file, "domain.punycode", "true")
        .expect("creates the section");
    assert_eq!(file.get_json("domain.punycode"), Some(Value::Bool(true)));
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
struct Limits {
    max: u32,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
struct Nested {
    min: u32,
    limits: Limits,
}

impl Default for Nested {
    fn default() -> Self {
        Self {
            min: 0,
            limits: Limits { max: 10 },
        }
    }
}

#[test]
fn unsetting_a_section_is_checked_like_unsetting_its_keys() {
    let schema = ConfigSchema::of::<Nested>().validate_with(|n: &Nested| {
        if n.min <= n.limits.max {
            Ok(())
        } else {
            Err("min exceeds max".to_owned())
        }
    });
    let mut file = ConfigFile::default();
    schema.set(&mut file, "limits.max", "30").expect("max");
    schema.set(&mut file, "min", "20").expect("min");
    let before = file.to_toml_string();
    let err = schema
        .unset(&mut file, "limits")
        .expect_err("default max 10 < min 20");
    assert!(err.to_string().contains("min exceeds max"), "{err}");
    assert_eq!(file.to_toml_string(), before, "file untouched");
    // An undeclared section is still removable.
    file.set("stale.x", "1").expect("hand edit");
    assert!(schema.unset(&mut file, "stale").expect("undeclared"));
}

#[test]
fn engine_value_errors_carry_a_code_and_fix() {
    for schema in [ConfigSchema::engine(), ConfigSchema::of::<Prefs>()] {
        let err = schema
            .set(&mut ConfigFile::default(), "credentials.store", "bogus")
            .expect_err("invalid");
        assert!(
            matches!(&err, CliCoreError::Fix { fix, .. } if !fix.is_empty()),
            "expected a fix, got {err:?}"
        );
    }
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "must deserialize from an empty file")]
fn of_rejects_settings_without_defaults() {
    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    struct Required {
        name: String,
    }
    drop(ConfigSchema::of::<Required>());
}

#[test]
fn defaults_follow_the_typed_document_when_a_parent_table_is_present_but_empty() {
    let schema = ConfigSchema::of::<Nested>();

    let absent = schema
        .entry(&ConfigFile::default(), "limits.max")
        .expect("entry");
    assert_eq!(
        absent.value,
        Value::from(10),
        "no [limits]: Nested::default() applies"
    );

    // `[limits]` exists, so `Limits` fills its own missing field: 0, not 10.
    let mut file = ConfigFile::default();
    file.set("limits.other", "1").expect("hand edit");
    let present = schema.entry(&file, "limits.max").expect("entry");
    assert_eq!(
        (present.value, present.source),
        (Value::from(0), SettingSource::Default)
    );
}

#[test]
fn removing_a_map_entry_is_checked() {
    #[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
    #[serde(default)]
    struct Weights {
        weights: std::collections::BTreeMap<String, u32>,
    }
    let schema = ConfigSchema::of::<Weights>().validate_with(|w: &Weights| {
        let total: u32 = w.weights.values().sum();
        if w.weights.is_empty() || total == 100 {
            Ok(())
        } else {
            Err(format!("weights total {total}"))
        }
    });
    let mut file = ConfigFile::default();
    file.set("weights.primary", "50").expect("hand edit");
    file.set("weights.secondary", "50").expect("hand edit");
    let before = file.to_toml_string();

    let err = schema
        .unset(&mut file, "weights.primary")
        .expect_err("total would be 50");
    assert!(err.to_string().contains("weights total 50"), "{err}");
    assert_eq!(file.to_toml_string(), before, "file untouched");
    assert!(
        schema
            .unset(&mut file, "weights")
            .expect("whole map is fine")
    );
}

#[test]
fn a_conflicting_parent_value_is_a_validation_error_with_a_fix() {
    let schema = ConfigSchema::of::<OptionalSection>();
    let mut file = ConfigFile::default();
    file.set("domain", "false").expect("hand edit");
    let err = schema
        .set(&mut file, "domain.punycode", "true")
        .expect_err("domain is not a table");
    assert!(
        matches!(&err, CliCoreError::Fix { fix, .. } if fix.contains("config file")),
        "expected a fix, got {err:?}"
    );
    assert_eq!(
        file.get_json("domain"),
        Some(Value::Bool(false)),
        "file untouched"
    );
}

/// Settings structs with field names that cannot be config keys.
mod bad_names {
    use super::*;

    #[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
    #[serde(default)]
    struct Dotted {
        #[serde(rename = "a.b")]
        flag: bool,
        fine: bool,
    }

    #[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
    #[serde(default)]
    struct Reserved {
        output: bool,
        fine: bool,
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "contains a '.'")]
    fn a_dotted_field_name_is_rejected_at_registration() {
        drop(ConfigSchema::of::<Dotted>());
    }

    #[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
    #[serde(default)]
    struct Empty {
        #[serde(rename = "")]
        flag: bool,
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "name is empty")]
    fn an_empty_field_name_is_rejected_at_registration() {
        drop(ConfigSchema::of::<Empty>());
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "reserved for the engine")]
    fn an_engine_reserved_field_name_is_rejected_at_registration() {
        drop(ConfigSchema::of::<Reserved>());
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn release_builds_omit_unusable_keys_instead_of_advertising_them() {
        let dotted = ConfigSchema::of::<Dotted>();
        assert!(dotted.find("a.b").is_none() && dotted.find("fine").is_some());
        let empty = ConfigSchema::of::<Empty>();
        assert!(empty.keys().iter().all(|k| !k.key.is_empty()));
        let reserved = ConfigSchema::of::<Reserved>();
        assert!(reserved.find("output").is_none() && reserved.find("fine").is_some());
        let mut file = ConfigFile::default();
        file.set("output.format", "json").expect("engine key");
        assert!(reserved.set(&mut file, "output", "true").is_err());
        assert_eq!(file.get_json("output.format"), Some(Value::from("json")));
    }
}

#[test]
fn a_configured_keys_default_is_what_applies_once_it_is_unset() {
    let schema = ConfigSchema::of::<Nested>();

    // Unsetting `limits.max` leaves `[limits]` (other = 1) behind, so the
    // handler would read `Limits::default()`'s 0, not `Nested::default()`'s 10.
    let mut retained = ConfigFile::default();
    retained.set("limits.max", "30").expect("hand edit");
    retained.set("limits.other", "1").expect("hand edit");
    let entry = schema.entry(&retained, "limits.max").expect("entry");
    assert_eq!(entry.key.default, Some(Value::from(0)));
    assert_eq!(entry.value, Value::from(30));

    // With nothing else in `[limits]`, unsetting prunes the table, so the
    // parent default applies.
    let mut pruned = ConfigFile::default();
    pruned.set("limits.max", "30").expect("hand edit");
    let entry = schema.entry(&pruned, "limits.max").expect("entry");
    assert_eq!(entry.key.default, Some(Value::from(10)));
}
