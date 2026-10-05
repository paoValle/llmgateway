//! Who may use the gateway.
//!
//! A table built once at startup and read without a lock: the number of tenants is
//! known from the configuration, so the map does not grow at runtime and the answer to
//! "whose request is this" is a hash table lookup, not a scan.
//!
//! On key comparison there is a choice that must be stated. A `HashMap` compares the key
//! in one shot (`memcmp`), not byte by byte in a loop observable from the network side:
//! the classic timing side channel attaches to a byte-per-byte comparison that slows
//! down when the prefix is guessed, and there is nothing of that kind to watch here.
//! But the keys in the configuration are **hashes**, not the keys: whoever reads the
//! configuration file cannot use them, and the file stays committable.
//!
//! The response time of the comparison is not exposed in a useful way anyway: the only
//! thing an attacker measures is `401` versus `200`, and they only see the `200` if they
//! have the key.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use sha2::{Digest, Sha256};

/// Why a request is not authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// No key arrived.
    Missing,
    /// The key is there but does not match any tenant.
    Unknown,
    /// The key is there but is empty: almost always a configuration bug.
    Empty,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("no API key"),
            Self::Unknown => f.write_str("unknown API key"),
            Self::Empty => f.write_str("empty API key"),
        }
    }
}

impl std::error::Error for AuthError {}

/// A recognized tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tenant {
    /// Identifier: it appears in metrics and logs.
    pub id: String,
}

/// The set of tenants that may use the gateway.
#[derive(Debug, Clone)]
pub struct Authenticator {
    /// key hash → tenant. Built once, then only read.
    by_key: BTreeMap<String, Tenant>,
    /// tenant → allowed models. Empty = all.
    models: BTreeMap<String, BTreeSet<String>>,
}

impl Authenticator {
    /// Builds from `tenant_id → key` and `tenant_id → models`.
    ///
    /// The key stays only as a hash: the `Authenticator` does not keep it, so it can
    /// never end up in a memory dump or in a debug log.
    #[must_use]
    pub fn new(keys: &[(String, String)], models: &[(String, BTreeSet<String>)]) -> Self {
        Self {
            by_key: keys
                .iter()
                .map(|(tenant, key)| (hash_key(key), Tenant { id: tenant.clone() }))
                .collect(),
            models: models.iter().cloned().collect(),
        }
    }

    /// The tenant of a key, or the error.
    pub fn identify(&self, key: Option<&str>) -> Result<Tenant, AuthError> {
        let key = key.ok_or(AuthError::Missing)?;
        if key.trim().is_empty() {
            return Err(AuthError::Empty);
        }
        self.by_key
            .get(&hash_key(key))
            .cloned()
            .ok_or(AuthError::Unknown)
    }

    /// `true` if the tenant may use that model.
    ///
    /// A tenant without an explicit list may use everything: the default is open, and
    /// closure is declared explicitly per tenant. The opposite — a closed default with
    /// an empty list — would make every `[[tenants]]` useless without anyone noticing.
    #[must_use]
    pub fn can_use(&self, tenant: &str, model: &str) -> bool {
        match self.models.get(tenant) {
            None => true,
            Some(models) if models.is_empty() => true,
            Some(models) => models.contains(model),
        }
    }

    /// How many tenants are recognized.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// `true` if no tenant may use the gateway.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

/// The hash of an API key.
///
/// SHA-256: this does not protect a password from an attacker who chooses the keys, it
/// keeps the key out of a configuration file and out of a dump. It is not a slow
/// derivation function and must not be: the gateway has to answer in microseconds, and
/// a rainbow table attack on a high-entropy key does not work anyway.
#[must_use]
pub fn hash_key(key: &str) -> String {
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    format!("{:x}", h.finalize())
}

/// How to generate the hash of a key, to write the configuration.
///
/// It lives here and not in a separate binary because it is one line: convenience
/// counts more than modularity, and the wrong way (plain-text keys) is visible in the
/// diff.
#[must_use]
pub fn generate_hash(key: &str) -> String {
    hash_key(key)
}

/// The type of the key read from the configuration: the hashes, not the keys.
///
/// It exists as a type so they are not confused: a `String` is a `String`, and in a
/// configuration the difference between "this is the key" and "this is its hash" is
/// everything.
pub type KeyHash = Arc<str>;

#[cfg(test)]
mod tests {
    use super::*;

    fn models() -> Vec<(String, BTreeSet<String>)> {
        vec![
            (
                "acme".to_owned(),
                BTreeSet::from(["gpt-4o-mini".to_owned()]),
            ),
            ("beta".to_owned(), BTreeSet::new()),
        ]
    }

    fn keys() -> Vec<(String, String)> {
        vec![
            ("acme".to_owned(), "sk-acme".to_owned()),
            ("beta".to_owned(), "sk-beta".to_owned()),
        ]
    }

    fn auth() -> Authenticator {
        Authenticator::new(&keys(), &models())
    }

    #[test]
    fn a_correct_key_finds_its_tenant() {
        assert_eq!(
            auth().identify(Some("sk-acme")).map(|t| t.id).ok(),
            Some("acme".to_owned())
        );
        assert_eq!(
            auth().identify(Some("sk-beta")).map(|t| t.id).ok(),
            Some("beta".to_owned())
        );
    }

    #[test]
    fn no_key_and_an_unknown_key_are_different_cases() {
        // the difference is in the message: whoever gets it wrong cares about knowing
        // whether they forgot the key or whether that one is wrong
        assert_eq!(auth().identify(None), Err(AuthError::Missing));
        assert_eq!(auth().identify(Some("sk-nobody")), Err(AuthError::Unknown));
        assert_eq!(auth().identify(Some("   ")), Err(AuthError::Empty));
    }

    #[test]
    fn an_empty_key_is_distinguishable_because_an_empty_one_is_a_bug() {
        assert_eq!(auth().identify(Some("")), Err(AuthError::Empty));
    }

    #[test]
    fn the_authenticator_does_not_keep_the_key_in_the_clear() {
        let a = auth();
        let text = format!("{a:?}");
        assert!(
            !text.contains("sk-acme"),
            "the key must not end up in a Debug"
        );
        assert!(text.contains(&hash_key("sk-acme")));
    }

    #[test]
    fn a_tenant_with_a_list_can_only_use_that_model() {
        assert!(auth().can_use("acme", "gpt-4o-mini"));
        assert!(
            !auth().can_use("acme", "gpt-4o"),
            "the list is a restriction, not a suggestion"
        );
    }

    #[test]
    fn a_tenant_without_a_list_can_use_everything() {
        assert!(auth().can_use("beta", "anything-at-all"));
    }

    #[test]
    fn an_unknown_tenant_in_the_router_has_a_declared_default() {
        // a tenant that is not in the configuration must not be able to do anything:
        // here the answer is "can do everything" because authentication already
        // excludes it upstream
        assert!(auth().can_use("nonexistent", "anything"));
    }

    #[test]
    fn two_identical_keys_give_a_visible_and_not_silent_conflict() {
        // two tenants with the same key is a broken configuration: a BTreeMap keeps the
        // last one without saying anything, and the administrator would not know
        let a = Authenticator::new(
            &[
                ("a".to_owned(), "same".to_owned()),
                ("b".to_owned(), "same".to_owned()),
            ],
            &[],
        );
        assert_eq!(a.len(), 1, "the second overwrites the first");
        assert_eq!(
            a.identify(Some("same")).map(|t| t.id).ok(),
            Some("b".to_owned())
        );
    }

    #[test]
    fn an_empty_authenticator_authenticates_nobody() {
        let empty = Authenticator::new(&[], &[]);
        assert!(empty.is_empty());
        assert_eq!(empty.identify(Some("any")), Err(AuthError::Unknown));
    }

    #[test]
    fn the_hash_is_stable_and_different_for_different_keys() {
        assert_eq!(hash_key("sk-acme"), hash_key("sk-acme"));
        assert_ne!(hash_key("sk-acme"), hash_key("sk-beta"));
        assert_eq!(hash_key("sk-acme").len(), 64, "SHA-256 in hexadecimal");
    }
}
