use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Cache TTL used when a credential has `cached_at`.
pub const CACHE_TTL: Duration = Duration::minutes(30);

/// Credential returned by an auth provider.
///
/// Field names and omission behavior match the provider JSON contract. Empty
/// strings are accepted because some providers omit optional values.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Credential {
    /// Access token used by transport injectors.
    #[serde(default)]
    pub token: String,
    /// Explicit expiration timestamp.
    #[serde(default)]
    pub expires_at: String,
    /// Cache creation timestamp. When present, [`CACHE_TTL`] determines expiry.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cached_at: String,
    /// Provider that produced this credential.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// Environment this credential targets.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub env: String,
    /// Environment alias accepted from provider responses.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub realm: String,
    /// Human-readable identity.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub identity: String,
    /// Subject identifier.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sub: String,
    /// Account type associated with the credential.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_type: String,
}

impl Credential {
    /// Returns the timestamp used for status display.
    #[must_use]
    pub fn effective_expiry(&self) -> String {
        if let Ok(cached_at) = DateTime::parse_from_rfc3339(&self.cached_at) {
            return (cached_at.with_timezone(&Utc) + CACHE_TTL)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        }
        self.expires_at.clone()
    }

    /// Reports whether the credential is expired.
    ///
    /// Invalid `expires_at` values are treated as expired. Credentials without
    /// either `expires_at` or `cached_at` are treated as not expired.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        if let Ok(cached_at) = DateTime::parse_from_rfc3339(&self.cached_at) {
            return Utc::now() > cached_at.with_timezone(&Utc) + CACHE_TTL;
        }
        if self.expires_at.is_empty() {
            return false;
        }
        match DateTime::parse_from_rfc3339(&self.expires_at) {
            Ok(expires_at) => Utc::now() > expires_at.with_timezone(&Utc),
            Err(_) => true,
        }
    }
}
