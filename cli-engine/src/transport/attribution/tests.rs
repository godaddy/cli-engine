use super::*;

const SECRET_SESSION: &str = "0b7e9c52-raw-session-id";

fn signals(env: &[(&str, &str)], interactive: bool) -> Signals {
    Signals::from_lookup(
        "gddy",
        |name| {
            env.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        },
        |_| false,
        interactive,
    )
}

fn claude_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("CLAUDECODE", "1"),
        ("CLAUDE_CODE_SESSION_ID", SECRET_SESSION),
    ]
}

fn resolve(signals: &Signals) -> Attribution {
    Attribution::resolve(&AttributionConfig::new(), "gddy", signals)
}

#[test]
fn detected_harness_adds_mode_and_agent_tokens() {
    let attribution = resolve(&signals(&claude_env(), false));

    assert_eq!(
        attribution.user_agent_suffix,
        " mode/agent agent/claude-code"
    );
}

#[test]
fn session_header_carries_a_hash_never_the_raw_id() {
    let attribution = resolve(&signals(&claude_env(), false));

    let value = attribution
        .headers
        .get("x-client-session")
        .expect("session header is sent");
    assert_eq!(value.len(), 16);
    assert!(value.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(!value.contains("raw-session"));
    assert!(!attribution.user_agent_suffix.contains(SECRET_SESSION));
}

#[test]
fn session_hash_is_stable_and_salted_per_app() {
    let same_a = hash_session("gddy", "claude-code", SECRET_SESSION);
    let same_b = hash_session("gddy", "claude-code", SECRET_SESSION);
    let other_app = hash_session("other-cli", "claude-code", SECRET_SESSION);
    let other_session = hash_session("gddy", "claude-code", "different");

    assert_eq!(same_a, same_b);
    assert_ne!(same_a, other_app);
    assert_ne!(same_a, other_session);
}

#[test]
fn harness_without_a_session_id_sends_tokens_but_no_header() {
    let attribution = resolve(&signals(&[("CLAUDECODE", "1")], false));

    assert_eq!(
        attribution.user_agent_suffix,
        " mode/agent agent/claude-code"
    );
    assert!(attribution.headers.is_empty());
}

#[test]
fn modes_follow_precedence_agent_ci_interactive_script() {
    assert_eq!(
        resolve(&signals(&[("CI", "true")], true)).user_agent_suffix,
        " mode/ci"
    );
    assert_eq!(
        resolve(&signals(&[], true)).user_agent_suffix,
        " mode/interactive"
    );
    assert_eq!(
        resolve(&signals(&[], false)).user_agent_suffix,
        " mode/script"
    );

    let mut agent_in_ci = claude_env();
    agent_in_ci.push(("CI", "true"));
    assert!(
        resolve(&signals(&agent_in_ci, true))
            .user_agent_suffix
            .starts_with(" mode/agent")
    );
}

#[test]
fn explicit_off_spellings_do_not_count_as_ci() {
    for value in ["", "  ", "0", "false", "FALSE", "no", "off"] {
        assert_eq!(
            resolve(&signals(&[("CI", value)], false)).user_agent_suffix,
            " mode/script",
            "CI={value:?}"
        );
    }
}

#[test]
fn app_scoped_env_var_opts_out_of_the_session_header_only() {
    let mut env = claude_env();
    env.push(("GDDY_NO_SESSION_ID", "1"));

    let attribution = resolve(&signals(&env, false));

    assert!(attribution.headers.is_empty());
    assert_eq!(
        attribution.user_agent_suffix,
        " mode/agent agent/claude-code"
    );
}

#[test]
fn opt_out_env_var_name_is_derived_from_the_app_id() {
    let env = [
        ("CLAUDECODE", "1"),
        ("CLAUDE_CODE_SESSION_ID", SECRET_SESSION),
        ("MY_CLI_NO_SESSION_ID", "true"),
    ];
    let lookup = |name: &str| {
        env.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
    };

    let signals = Signals::from_lookup("my-cli", lookup, |_| false, false);

    assert!(signals.session_opted_out);
}

#[test]
fn config_can_disable_the_session_header() {
    let config = AttributionConfig::new().without_session_id();

    let attribution = Attribution::resolve(&config, "gddy", &signals(&claude_env(), false));

    assert!(attribution.headers.is_empty());
    assert!(attribution.user_agent_suffix.contains("agent/claude-code"));
}

#[test]
fn config_can_rename_the_session_header() {
    let config = AttributionConfig::new().with_session_header("X-GDDY-Session");

    let attribution = Attribution::resolve(&config, "gddy", &signals(&claude_env(), false));

    assert!(attribution.headers.contains_key("x-gddy-session"));
    assert!(!attribution.headers.contains_key("x-client-session"));
}

#[test]
fn invalid_header_name_drops_the_header_but_keeps_the_tokens() {
    let config = AttributionConfig::new().with_session_header("not a header");

    let attribution = Attribution::resolve(&config, "gddy", &signals(&claude_env(), false));

    assert!(attribution.headers.is_empty());
    assert!(attribution.user_agent_suffix.contains("mode/agent"));
}
