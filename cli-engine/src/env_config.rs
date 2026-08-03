//! Declarative, attribute-driven per-environment configuration.
//!
//! An [`EnvConfig`] struct describes, per field, where to find its value —
//! a TOML key, an opt-in environment-variable suffix, a literal or computed
//! default, and (when the defaults don't fit) a custom conversion function
//! for either raw form. `#[derive(EnvConfig)]` generates the wiring; see
//! `docs/environments.md` for the attribute grammar and
//! [`crate::environments::Environments::resolve`] for the common way to
//! build a [`SourceChain`] and assemble a struct from it.
//!
//! # Why `toml::Value`, and why it's exposed directly
//!
//! Every source except an environment variable represents its values as
//! `toml::Value`/`toml::Table` — not a `cli_engine`-owned wrapper type.
//! Deliberate, not incidental:
//!
//! - The file layer (`environments.toml`) genuinely *is* TOML; parsing it
//!   already produces a `toml::Table`.
//! - Compiled-in and in-memory sources ([`crate::environments::EnvTable`],
//!   [`ValueSource`]) use that *same* representation so they merge with the
//!   file layer key-by-key with no translation between "code-supplied" and
//!   "file-supplied" shapes, and so a field's default conversion
//!   ([`default_from_toml`](crate::env_config::default_from_toml)) can lean on `toml::Value`'s own
//!   `serde::Deserialize` impl — a TOML array becomes a `Vec<String>`
//!   natively, a TOML table becomes a nested struct, with no per-field
//!   stringly-typed detour.
//! - An environment variable is the one genuine exception: it's always a
//!   plain `String` (an OS-level constraint, not a design choice), so
//!   [`ConfigSource::env_var`] returns `Option<String>`, never
//!   `Option<toml::Value>`, and a field's `from_env` conversion is always a
//!   separate function from its `from_toml` conversion.
//!
//! Using `toml::Value`/`toml::Table` directly, instead of hiding them behind
//! a `cli_engine`-owned newtype, means they're part of this crate's public
//! API — and a consumer whose `toml::Value` came from its *own* direct
//! dependency, rather than this re-export, would need that dependency on
//! the same major version as this crate's, or the two crates' `toml::Value`
//! types are different, incompatible types despite sharing a name.
//! [`#[derive(EnvConfig)]`](cli_engine_macros::EnvConfig)-generated code
//! always goes through [`crate::env_config::toml`] rather than a bare
//! `toml::` path for exactly this reason, so the common case (no custom
//! `from_toml`/`to_toml`) needs no direct `toml` dependency in the consumer
//! at all. A consumer writing a custom `from_toml`/`to_toml` function or
//! calling [`ValueSource::with`] directly should do the same —
//! `cli_engine::env_config::toml::Value`, not its own `toml` dependency's
//! `toml::Value` — to get the same guarantee.

use std::fmt;

pub use cli_engine_macros::EnvConfig;
/// Re-exported so `#[derive(EnvConfig)]`-generated code (and a consumer's
/// own `from_toml`/`to_toml` functions) can name `toml::Value`/`toml::Table`
/// through `cli_engine` itself rather than needing a direct `toml`
/// dependency of their own — see the module-level "Why `toml::Value`" note
/// above.
pub use toml;

/// Something a field's assembly instructions can be checked against: "do you
/// have a TOML-shaped value for this key" and "do you have a string value for
/// an env var with this suffix." [`EnvConfig::assemble`] walks a whole
/// [`SourceChain`] of these, in priority order, so more than one kind of
/// fallback source can contribute to the same struct.
pub trait ConfigSource {
    /// The raw TOML value stored under `key`, if this source has one.
    fn toml_value(&self, key: &str) -> Option<&toml::Value>;

    /// The raw environment-variable string for a field, if this source has one.
    fn env_var(&self, suffix: &str) -> Option<String> {
        let _ = suffix;
        None
    }

    /// The environment name this source represents, if it represents one at
    /// all.
    fn env_name(&self) -> Option<&str> {
        None
    }
}

/// One environment's source: its name and its merged TOML table (compiled-in
/// table overlaid by the `environments.toml` file table). Purely a TOML lookup;
/// env-var overrides are handled by a separate [`EnvVarSource`] pushed
/// alongside it — see [`crate::environments::Environments::resolve`].
#[derive(Debug, Clone)]
pub struct EnvSource {
    name: String,
    table: toml::Table,
}

impl EnvSource {
    /// Creates a source from an already-merged table.
    #[must_use]
    pub fn new(name: impl Into<String>, table: toml::Table) -> Self {
        Self {
            name: name.into(),
            table,
        }
    }

    /// The environment name this source was resolved for.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The whole merged table, for generic introspection.
    #[must_use]
    pub fn table(&self) -> &toml::Table {
        &self.table
    }
}

impl ConfigSource for EnvSource {
    fn toml_value(&self, key: &str) -> Option<&toml::Value> {
        self.table.get(key)
    }

    fn env_name(&self) -> Option<&str> {
        Some(&self.name)
    }
}

/// A source that only ever answers environment-variable lookups, under a
/// fixed prefix — the app id for the common case.
#[derive(Debug, Clone)]
pub struct EnvVarSource {
    /// The prefix prepended to a field's `env` suffix, e.g. `"GDDY"` for
    /// `GDDY_<SUFFIX>`.
    pub prefix: String,
}

impl ConfigSource for EnvVarSource {
    fn toml_value(&self, _key: &str) -> Option<&toml::Value> {
        None
    }

    fn env_var(&self, suffix: &str) -> Option<String> {
        std::env::var(format!("{}_{suffix}", self.prefix)).ok()
    }
}

/// A source backed by values a consumer already has in hand — for example a
/// provider's own constructor arguments, used as a last-resort fallback tier.
#[derive(Debug, Clone, Default)]
pub struct ValueSource(toml::Table);

impl ValueSource {
    /// Creates an empty value source.
    #[must_use]
    pub fn new() -> Self {
        Self(toml::Table::new())
    }

    /// Sets `key` to `value`, converting via [`Into<toml::Value>`].
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<toml::Value>) -> Self {
        self.0.insert(key.into(), value.into());
        self
    }
}

impl ConfigSource for ValueSource {
    fn toml_value(&self, key: &str) -> Option<&toml::Value> {
        self.0.get(key)
    }
}

/// An ordered chain of [`ConfigSource`]s. Assembly walks the chain in order;
/// within one source its env var is checked before its TOML value; the first
/// source to answer either wins for that field. Build one with
/// [`SourceChain::new`] and [`SourceChain::push`], then pass it to
/// [`EnvConfig::assemble`].
#[derive(Default)]
pub struct SourceChain<'src>(Vec<&'src dyn ConfigSource>);

impl fmt::Debug for SourceChain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceChain")
            .field("len", &self.0.len())
            .finish()
    }
}

impl<'src> SourceChain<'src> {
    /// Creates an empty chain.
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Appends `source` to the end of the chain (lowest priority so far).
    #[must_use]
    pub fn push(mut self, source: &'src dyn ConfigSource) -> Self {
        self.0.push(source);
        self
    }

    /// Iterates the chain in priority order (first pushed, checked first).
    pub fn iter(&self) -> impl Iterator<Item = &'src dyn ConfigSource> + '_ {
        self.0.iter().copied()
    }

    /// The first source in the chain that has a TOML value for `key`, if
    /// any. A `default_fn` uses this to derive its field from a *sibling*
    /// field's raw value (for example deriving `auth_url` from `api_url`)
    /// without duplicating the chain-walk `resolve_field` already does.
    #[must_use]
    pub fn toml_value(&self, key: &str) -> Option<&toml::Value> {
        self.iter().find_map(|source| source.toml_value(key))
    }

    /// The environment name of the first source in the chain that has one
    /// (see [`ConfigSource::env_name`]), if any. A `default_fn` uses this to
    /// derive its field from the environment's own identity (for example
    /// `account.{name}.example.com`).
    #[must_use]
    pub fn env_name(&self) -> Option<&str> {
        self.iter().find_map(|source| source.env_name())
    }
}

/// A struct that can be assembled from a [`SourceChain`] — implemented by
/// `#[derive(EnvConfig)]`.
pub trait EnvConfig: Sized {
    /// Threads `sources` through this struct's per-field assembly
    /// instructions to build an instance.
    ///
    /// # Errors
    ///
    /// Returns [`EnvConfigError::InvalidField`] when a present value fails to
    /// convert to its field's type, or [`EnvConfigError::MissingField`] when
    /// no source has a value and the field has no default.
    fn assemble(sources: &SourceChain<'_>) -> Result<Self, EnvConfigError>;
}

/// An error assembling an [`EnvConfig`] struct.
#[derive(Debug, thiserror::Error)]
pub enum EnvConfigError {
    /// A present value failed to convert to the field's type.
    #[error("field {field}: {reason}")]
    InvalidField {
        /// The field's Rust name.
        field: &'static str,
        /// Why conversion failed.
        reason: String,
    },
    /// No source had a value for this field, and it has no default.
    #[error("field {field} has no value in any source, and no default")]
    MissingField {
        /// The field's Rust name.
        field: &'static str,
    },
    /// The underlying environment failed to resolve at all (unknown name,
    /// unreadable/malformed `environments.toml`).
    #[error(transparent)]
    Environment(Box<crate::error::CliCoreError>),
}

// A hand-written `From` (rather than thiserror's `#[from]`) so `?` converts a
// plain `CliCoreError` directly — `CliCoreError` in turn converts *from*
// `EnvConfigError` (see error.rs), and thiserror's `#[from]` would otherwise
// need the boxed type on both sides of that cycle to keep each enum's size
// finite, which breaks the ergonomic `?` conversion callers expect.
impl From<crate::error::CliCoreError> for EnvConfigError {
    fn from(err: crate::error::CliCoreError) -> Self {
        Self::Environment(Box::new(err))
    }
}

/// Default TOML-to-`T` conversion used when a field has no `from_toml`
/// attribute: `T` must be [`serde::de::DeserializeOwned`].
///
/// # Errors
///
/// Returns an error when `value` doesn't match `T`'s shape.
pub fn default_from_toml<T: serde::de::DeserializeOwned>(value: &toml::Value) -> Result<T, String> {
    value.clone().try_into::<T>().map_err(|err| err.to_string())
}

/// Default string-to-`T` conversion used when a field has no `from_env`
/// attribute: `T` must implement [`std::str::FromStr`].
///
/// # Errors
///
/// Returns an error when `raw` doesn't parse as `T`.
pub fn default_from_env<T>(raw: &str) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: fmt::Display,
{
    raw.parse::<T>().map_err(|err| err.to_string())
}

/// Walks `sources` looking for a value for one field: within a source, its
/// env var (if `env_suffix` is given) is checked before its TOML value; the
/// first source to answer either wins. Returns `Ok(None)` when no source has
/// the field at all, letting the caller apply a default.
///
/// By default (`allow_blank` is `false`), a source that answers with an
/// empty-or-whitespace-only string, or an empty TOML array, is treated the
/// same as that source not answering at all: `resolve_field` keeps walking
/// the rest of the chain, trying the *next* source, and ultimately falls to
/// the field's `default`/`default_fn` only once every source has been asked.
/// (An env var is always a string; a TOML value counts only when it *is* a
/// string or an array — any other TOML shape, including an empty table,
/// never counts as blank.) This fits nearly every field — a blank or empty
/// override is essentially always a mistake or an unset placeholder, not a
/// real value, and this holds whether a field has one source or several
/// fallback tiers. Set `allow_blank` on the rare field where `""` or `[]` is
/// itself a meaningful, literal answer.
///
/// # Errors
///
/// Returns [`EnvConfigError::InvalidField`] when a present, non-blank value
/// fails `from_env`/`from_toml`.
pub fn resolve_field<T>(
    sources: &SourceChain<'_>,
    field: &'static str,
    key: &str,
    env_suffix: Option<&str>,
    allow_blank: bool,
    from_toml: impl Fn(&toml::Value) -> Result<T, String>,
    from_env: impl Fn(&str) -> Result<T, String>,
) -> Result<Option<T>, EnvConfigError> {
    fn is_blank(s: &str) -> bool {
        s.trim().is_empty()
    }
    fn is_blank_toml_value(value: &toml::Value) -> bool {
        match value {
            toml::Value::String(s) => is_blank(s),
            toml::Value::Array(a) => a.is_empty(),
            _ => false,
        }
    }

    for source in sources.iter() {
        if let Some(suffix) = env_suffix
            && let Some(raw) = source.env_var(suffix)
            && (allow_blank || !is_blank(&raw))
        {
            return from_env(&raw)
                .map(Some)
                .map_err(|reason| EnvConfigError::InvalidField { field, reason });
        }
        if let Some(value) = source.toml_value(key)
            && (allow_blank || !is_blank_toml_value(value))
        {
            return from_toml(value)
                .map(Some)
                .map_err(|reason| EnvConfigError::InvalidField { field, reason });
        }
    }
    Ok(None)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct Section {
        client_id: String,
        port: u32,
    }

    impl EnvConfig for Section {
        fn assemble(sources: &SourceChain<'_>) -> Result<Self, EnvConfigError> {
            let client_id = match resolve_field::<String>(
                sources,
                "client_id",
                "client_id",
                None,
                false,
                default_from_toml::<String>,
                default_from_env::<String>,
            )? {
                Some(v) => v,
                None => {
                    return Err(EnvConfigError::MissingField { field: "client_id" });
                }
            };
            let port = resolve_field::<u32>(
                sources,
                "port",
                "port",
                Some("PORT"),
                false,
                default_from_toml::<u32>,
                default_from_env::<u32>,
            )?
            .unwrap_or(8080);
            Ok(Self { client_id, port })
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct WithDerivedField {
        base: String,
        derived: String,
        env_name: String,
    }

    impl EnvConfig for WithDerivedField {
        fn assemble(sources: &SourceChain<'_>) -> Result<Self, EnvConfigError> {
            let base = resolve_field::<String>(
                sources,
                "base",
                "base",
                None,
                true, // allow_blank: opts out of the default, accepting "" literally
                default_from_toml::<String>,
                default_from_env::<String>,
            )?
            .unwrap_or_default();
            let derived = match resolve_field::<String>(
                sources,
                "derived",
                "derived",
                None,
                false, // allow_blank: default — blank collapses to absent
                default_from_toml::<String>,
                default_from_env::<String>,
            )? {
                Some(v) => v,
                None => format!("derived-from-{base}"),
            };
            let env_name = sources.env_name().unwrap_or_default().to_owned();
            Ok(Self {
                base,
                derived,
                env_name,
            })
        }
    }

    #[test]
    fn toml_value_wins_when_no_env_var_set() {
        let mut table = toml::Table::new();
        table.insert("client_id".to_owned(), "from-toml".into());
        let env = EnvSource::new("prod", table);
        let chain = SourceChain::new().push(&env);
        let section = Section::assemble(&chain).expect("assembles");
        assert_eq!(section.client_id, "from-toml");
        assert_eq!(section.port, 8080, "no source set port; default applies");
    }

    #[test]
    fn env_var_source_outranks_toml_value_source() {
        let mut table = toml::Table::new();
        table.insert("client_id".to_owned(), "from-toml".into());
        table.insert("port".to_owned(), toml::Value::Integer(1234));
        let env = EnvSource::new("prod", table);

        // SAFETY: single-threaded test, no other test reads this var.
        unsafe { std::env::set_var("GDDY_PORT", "9999") };
        let app = EnvVarSource {
            prefix: "GDDY".to_owned(),
        };
        let chain = SourceChain::new().push(&app).push(&env);
        let section = Section::assemble(&chain).expect("assembles");
        // SAFETY: matches the set_var above.
        unsafe { std::env::remove_var("GDDY_PORT") };

        assert_eq!(
            section.client_id, "from-toml",
            "app source has no client_id, falls through to env table"
        );
        assert_eq!(
            section.port, 9999,
            "app-scoped env var outranks the TOML value"
        );
    }

    #[test]
    fn missing_required_field_errors() {
        let chain = SourceChain::new();
        let err = Section::assemble(&chain).unwrap_err();
        assert!(matches!(
            err,
            EnvConfigError::MissingField { field: "client_id" }
        ));
    }

    #[test]
    fn malformed_value_is_a_hard_error() {
        let mut table = toml::Table::new();
        table.insert("client_id".to_owned(), "ok".into());
        table.insert("port".to_owned(), "not-a-number".into());
        let env = EnvSource::new("prod", table);
        let chain = SourceChain::new().push(&env);
        let err = Section::assemble(&chain).unwrap_err();
        assert!(matches!(
            err,
            EnvConfigError::InvalidField { field: "port", .. }
        ));
    }

    #[test]
    fn value_source_is_a_pure_table_lookup() {
        let base = ValueSource::new().with("client_id", "base-client");
        let chain = SourceChain::new().push(&base);
        let section = Section::assemble(&chain).expect("assembles");
        assert_eq!(section.client_id, "base-client");
    }

    #[test]
    fn default_fn_derives_from_a_sibling_field_via_source_chain_toml_value() {
        let mut table = toml::Table::new();
        table.insert("base".to_owned(), "widget".into());
        let env = EnvSource::new("prod", table);
        let chain = SourceChain::new().push(&env);
        let section = WithDerivedField::assemble(&chain).expect("assembles");
        assert_eq!(section.derived, "derived-from-widget");
    }

    #[test]
    fn blank_value_falls_through_to_default_fn_by_default() {
        let mut table = toml::Table::new();
        table.insert("base".to_owned(), "widget".into());
        table.insert("derived".to_owned(), "   ".into());
        let env = EnvSource::new("prod", table);
        let chain = SourceChain::new().push(&env);
        let section = WithDerivedField::assemble(&chain).expect("assembles");
        assert_eq!(
            section.derived, "derived-from-widget",
            "an explicit blank value is treated the same as an absent one by default"
        );
    }

    #[test]
    fn allow_blank_accepts_a_blank_value_as_is() {
        let mut table = toml::Table::new();
        table.insert("base".to_owned(), "   ".into());
        let env = EnvSource::new("prod", table);
        let chain = SourceChain::new().push(&env);
        let section = WithDerivedField::assemble(&chain).expect("assembles");
        assert_eq!(
            section.base, "   ",
            "`base` opts in via `allow_blank`, so its blank value is used as-is"
        );
    }

    #[test]
    fn empty_array_falls_through_to_the_next_source_by_default() {
        // Mirrors the blank-string case, but for a Vec<T> field: a source
        // that answers with an empty TOML array (e.g. a wired environment's
        // `scopes = []`) must not silently win over a later tier's real
        // value — same "blank is absent by default" rule, extended to
        // arrays, not just strings.
        let mut table = toml::Table::new();
        table.insert("tags".to_owned(), toml::Value::Array(Vec::new()));
        let env = EnvSource::new("prod", table);
        let base = ValueSource::new().with("tags", vec!["real".to_owned()]);
        let chain = SourceChain::new().push(&env).push(&base);

        let tags = resolve_field::<Vec<String>>(
            &chain,
            "tags",
            "tags",
            None,
            false, // allow_blank: default — empty array collapses to absent
            default_from_toml::<Vec<String>>,
            |_raw: &str| -> Result<Vec<String>, String> { Err(String::new()) },
        )
        .expect("resolves")
        .expect("some tier has a value");

        assert_eq!(
            tags,
            vec!["real".to_owned()],
            "the higher-priority source's empty array must defer to the base's real value"
        );
    }

    #[test]
    fn allow_blank_accepts_an_empty_array_as_is() {
        let mut table = toml::Table::new();
        table.insert("tags".to_owned(), toml::Value::Array(Vec::new()));
        let env = EnvSource::new("prod", table);
        let base = ValueSource::new().with("tags", vec!["real".to_owned()]);
        let chain = SourceChain::new().push(&env).push(&base);

        let tags = resolve_field::<Vec<String>>(
            &chain,
            "tags",
            "tags",
            None,
            true, // allow_blank: opts out, accepting [] literally
            default_from_toml::<Vec<String>>,
            |_raw: &str| -> Result<Vec<String>, String> { Err(String::new()) },
        )
        .expect("resolves")
        .expect("some tier has a value");

        assert_eq!(
            tags,
            Vec::<String>::new(),
            "with allow_blank set, the empty array is accepted as-is, not skipped"
        );
    }

    #[test]
    fn source_chain_env_name_reflects_the_first_env_source() {
        let table = toml::Table::new();
        let env = EnvSource::new("staging", table);
        let chain = SourceChain::new().push(&env);
        let section = WithDerivedField::assemble(&chain).expect("assembles");
        assert_eq!(section.env_name, "staging");

        let base = ValueSource::new();
        let no_env_chain = SourceChain::new().push(&base);
        let section = WithDerivedField::assemble(&no_env_chain).expect("assembles");
        assert_eq!(
            section.env_name, "",
            "a chain with no EnvSource has no env name to report"
        );
    }
}
