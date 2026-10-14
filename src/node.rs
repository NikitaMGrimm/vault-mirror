//! A decrypted JSON tree that remembers which strings were ciphertext.
//!
//! Vault items are handled without a schema: every string that is an encrypted
//! value becomes a [`Node::Secret`], everything else is kept as is. Writing an
//! item back encrypts exactly the secrets again. A new item type or field that
//! a server introduces is therefore mirrored without a code change.

use std::collections::BTreeMap;

use serde_json::{Map, Number, Value};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

use crate::crypto::{CryptoError, SymKey, looks_encrypted};

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Null,
    Bool(bool),
    Number(Number),
    /// A string the server stores in the clear, such as a date.
    Text(String),
    /// A string that is encrypted at rest. Holds the plaintext.
    Secret(String),
    List(Vec<Node>),
    Map(BTreeMap<String, Node>),
}

impl Node {
    pub fn secret(value: impl Into<String>) -> Self {
        Self::Secret(value.into())
    }

    pub fn decrypt(value: &Value, key: &SymKey) -> Result<Self, CryptoError> {
        Ok(match value {
            Value::Null => Self::Null,
            Value::Bool(b) => Self::Bool(*b),
            Value::Number(n) => Self::Number(n.clone()),
            Value::String(s) if looks_encrypted(s) => Self::Secret(key.decrypt_str(s)?),
            Value::String(s) => Self::Text(s.clone()),
            Value::Array(items) => Self::List(
                items
                    .iter()
                    .map(|item| Self::decrypt(item, key))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Object(map) => Self::Map(
                map.iter()
                    .map(|(k, v)| Ok((k.clone(), Self::decrypt(v, key)?)))
                    .collect::<Result<_, CryptoError>>()?,
            ),
        })
    }

    pub fn encrypt(&self, key: &SymKey) -> Result<Value, CryptoError> {
        Ok(match self {
            Self::Null => Value::Null,
            Self::Bool(b) => Value::Bool(*b),
            Self::Number(n) => Value::Number(n.clone()),
            Self::Text(s) => Value::String(s.clone()),
            Self::Secret(s) => Value::String(key.encrypt_str(s)?),
            Self::List(items) => Value::Array(
                items
                    .iter()
                    .map(|item| item.encrypt(key))
                    .collect::<Result<_, _>>()?,
            ),
            Self::Map(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| Ok((k.clone(), v.encrypt(key)?)))
                    .collect::<Result<Map<_, _>, CryptoError>>()?,
            ),
        })
    }

    /// The tree as ordinary JSON with secrets in the clear, for exports.
    pub fn to_plain(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(b) => Value::Bool(*b),
            Self::Number(n) => Value::Number(n.clone()),
            Self::Text(s) | Self::Secret(s) => Value::String(s.clone()),
            Self::List(items) => Value::Array(items.iter().map(Self::to_plain).collect()),
            Self::Map(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), v.to_plain()))
                    .collect(),
            ),
        }
    }

    /// A canonical form for comparing the same item on two servers.
    ///
    /// Servers differ in how they echo an item back: one drops a field that is
    /// null, another returns an empty list, and timestamps change precision.
    /// None of that is a content change, so empty values are removed and
    /// timestamps are brought to one format. `None` means "no content".
    pub fn normalized(&self) -> Option<Value> {
        match self {
            Self::Null => None,
            Self::Bool(b) => Some(Value::Bool(*b)),
            Self::Number(n) => Some(Value::Number(n.clone())),
            Self::Secret(s) => (!s.is_empty()).then(|| Value::String(s.clone())),
            Self::Text(s) => {
                (!s.is_empty()).then(|| Value::String(normalize_timestamp(s).unwrap_or_else(|| s.clone())))
            }
            Self::List(items) => {
                let items: Vec<Value> = items.iter().filter_map(Self::normalized).collect();
                (!items.is_empty()).then_some(Value::Array(items))
            }
            Self::Map(map) => {
                let map: Map<String, Value> = map
                    .iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.normalized()?)))
                    .collect();
                (!map.is_empty()).then_some(Value::Object(map))
            }
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(s) | Self::Secret(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Node]> {
        match self {
            Self::List(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&BTreeMap<String, Node>> {
        match self {
            Self::Map(map) => Some(map),
            _ => None,
        }
    }
}

/// Rewrites an RFC 3339 timestamp as UTC with millisecond precision.
fn normalize_timestamp(value: &str) -> Option<String> {
    // Cheap reject: "YYYY-MM-DDTHH:MM:SS" at minimum.
    let bytes = value.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let parsed = OffsetDateTime::parse(value, &Rfc3339).ok()?.to_offset(UtcOffset::UTC);
    let truncated = parsed.replace_millisecond(parsed.millisecond()).ok()?;
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        truncated.year(),
        u8::from(truncated.month()),
        truncated.day(),
        truncated.hour(),
        truncated.minute(),
        truncated.second(),
        truncated.millisecond()
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn map(entries: Vec<(&str, Node)>) -> Node {
        Node::Map(entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    #[test]
    fn decrypting_marks_exactly_the_encrypted_strings() {
        let key = SymKey::generate();
        let raw = json!({
            "name": key.encrypt_str("GitHub").unwrap(),
            "type": 1,
            "favorite": false,
            "revisionDate": "2026-01-02T03:04:05.000000Z",
            "login": {
                "username": key.encrypt_str("octocat").unwrap(),
                "uris": [{"uri": key.encrypt_str("https://github.com").unwrap(), "match": null}],
            },
        });
        let node = Node::decrypt(&raw, &key).unwrap();
        let expected = map(vec![
            ("name", Node::secret("GitHub")),
            ("type", Node::Number(1.into())),
            ("favorite", Node::Bool(false)),
            ("revisionDate", Node::Text("2026-01-02T03:04:05.000000Z".into())),
            (
                "login",
                map(vec![
                    ("username", Node::secret("octocat")),
                    (
                        "uris",
                        Node::List(vec![map(vec![
                            ("uri", Node::secret("https://github.com")),
                            ("match", Node::Null),
                        ])]),
                    ),
                ]),
            ),
        ]);
        assert_eq!(node, expected);
    }

    #[test]
    fn encrypting_round_trips_through_another_key() {
        let source = SymKey::generate();
        let dest = SymKey::generate();
        let raw = json!({"name": source.encrypt_str("n").unwrap(), "date": "2026-01-02T03:04:05Z", "n": 3});
        let node = Node::decrypt(&raw, &source).unwrap();

        let reencrypted = node.encrypt(&dest).unwrap();
        assert_ne!(reencrypted["name"], raw["name"]);
        assert_eq!(reencrypted["date"], raw["date"]);
        assert_eq!(Node::decrypt(&reencrypted, &dest).unwrap(), node);
        assert!(Node::decrypt(&reencrypted, &source).is_err());
    }

    #[test]
    fn a_value_that_looks_encrypted_but_does_not_decrypt_is_an_error() {
        let raw = json!({"name": SymKey::generate().encrypt_str("n").unwrap()});
        assert!(Node::decrypt(&raw, &SymKey::generate()).is_err());
    }

    #[test]
    fn plain_export_reveals_secrets() {
        let node = map(vec![("name", Node::secret("n")), ("d", Node::Text("x".into()))]);
        assert_eq!(node.to_plain(), json!({"name": "n", "d": "x"}));
    }

    #[test]
    fn normalizing_drops_everything_that_carries_no_content() {
        let node = map(vec![
            ("name", Node::secret("n")),
            ("notes", Node::Null),
            ("emptyNotes", Node::secret("")),
            ("fields", Node::List(vec![])),
            ("card", map(vec![("number", Node::Null)])),
            ("uris", Node::List(vec![Node::Null, map(vec![("uri", Node::secret("u"))])])),
            ("favorite", Node::Bool(false)),
            ("reprompt", Node::Number(0.into())),
        ]);
        assert_eq!(
            node.normalized().unwrap(),
            json!({"name": "n", "uris": [{"uri": "u"}], "favorite": false, "reprompt": 0})
        );
        assert_eq!(map(vec![("a", Node::Null)]).normalized(), None);
    }

    #[test]
    fn timestamps_compare_equal_across_server_formats() {
        let variants = [
            "2026-01-02T03:04:05.678Z",
            "2026-01-02T03:04:05.678000Z",
            "2026-01-02T03:04:05.6789999Z",
            "2026-01-02T04:04:05.678+01:00",
        ];
        for variant in variants {
            assert_eq!(
                Node::Text(variant.into()).normalized().unwrap(),
                json!("2026-01-02T03:04:05.678Z"),
                "{variant}"
            );
        }
        assert_eq!(
            Node::Text("2026-01-02T03:04:05Z".into()).normalized().unwrap(),
            json!("2026-01-02T03:04:05.000Z")
        );
    }

    #[test]
    fn only_plain_timestamps_are_rewritten() {
        // A secret that happens to look like a timestamp is user content.
        assert_eq!(
            Node::secret("2026-01-02T03:04:05Z").normalized().unwrap(),
            json!("2026-01-02T03:04:05Z")
        );
        for plain in ["not a date", "2026-01-02", "9a1c7c5e-0c1f-4a4b-9d39-0c1c2f7f7a10"] {
            assert_eq!(Node::Text(plain.into()).normalized().unwrap(), json!(plain));
        }
    }
}
