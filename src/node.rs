//! A decrypted JSON tree that remembers which strings were ciphertext.
//!
//! Vault items are handled without a schema: every string that is an encrypted
//! value becomes a [`Node::Secret`], everything else is kept as is. Writing an
//! item back encrypts exactly the secrets again. A new item type or field that
//! a server introduces is therefore mirrored without a code change.

use std::{collections::BTreeMap, fmt};

use serde_json::{Map, Number, Value};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

use crate::crypto::{CryptoError, SymKey, looks_encrypted};

#[derive(Clone, PartialEq)]
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

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("Null"),
            Self::Bool(value) => f.debug_tuple("Bool").field(value).finish(),
            Self::Number(value) => f.debug_tuple("Number").field(value).finish(),
            Self::Text(value) => f.debug_tuple("Text").field(value).finish(),
            Self::Secret(_) => f.debug_tuple("Secret").field(&"[redacted]").finish(),
            Self::List(values) => f.debug_tuple("List").field(values).finish(),
            Self::Map(values) => f.debug_tuple("Map").field(values).finish(),
        }
    }
}

impl Node {
    pub fn secret(value: impl Into<String>) -> Self {
        Self::Secret(value.into())
    }

    /// Errors name the JSON pointer of the failing string within `value`.
    pub fn decrypt(value: &Value, key: &SymKey) -> Result<Self, CryptoError> {
        fn walk(value: &Value, key: &SymKey, path: &mut String) -> Result<Node, CryptoError> {
            Ok(match value {
                Value::Null => Node::Null,
                Value::Bool(b) => Node::Bool(*b),
                Value::Number(n) => Node::Number(n.clone()),
                Value::String(s) if looks_encrypted(s) => {
                    Node::Secret(key.decrypt_str(s).map_err(|error| match error {
                        CryptoError::Malformed { .. } => {
                            CryptoError::MalformedValue { path: path.clone() }
                        }
                        _ => CryptoError::DecryptValue { path: path.clone() },
                    })?)
                }
                Value::String(s) => Node::Text(s.clone()),
                Value::Array(items) => Node::List(
                    items
                        .iter()
                        .enumerate()
                        .map(|(index, item)| {
                            let length = path.len();
                            path.push_str(&format!("/{index}"));
                            let node = walk(item, key, path);
                            path.truncate(length);
                            node
                        })
                        .collect::<Result<_, _>>()?,
                ),
                Value::Object(map) => Node::Map(
                    map.iter()
                        .map(|(k, v)| {
                            let length = path.len();
                            path.push('/');
                            path.push_str(&k.replace('~', "~0").replace('/', "~1"));
                            let node = walk(v, key, path);
                            path.truncate(length);
                            Ok((k.clone(), node?))
                        })
                        .collect::<Result<_, CryptoError>>()?,
                ),
            })
        }
        walk(value, key, &mut String::new())
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
            Self::Map(map) => {
                Value::Object(map.iter().map(|(k, v)| (k.clone(), v.to_plain())).collect())
            }
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
            Self::Text(s) => (!s.is_empty())
                .then(|| Value::String(normalize_timestamp(s).unwrap_or_else(|| s.clone()))),
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

    /// JSON pointers to every string that is stored in the clear.
    ///
    /// Together with [`Node::to_plain`] this is enough to rebuild the tree,
    /// because every other string is a secret.
    pub fn plain_string_paths(&self) -> Vec<String> {
        fn walk(node: &Node, path: &mut String, out: &mut Vec<String>) {
            match node {
                Node::Text(_) => out.push(path.clone()),
                Node::List(items) => {
                    for (index, item) in items.iter().enumerate() {
                        let length = path.len();
                        path.push_str(&format!("/{index}"));
                        walk(item, path, out);
                        path.truncate(length);
                    }
                }
                Node::Map(map) => {
                    for (key, value) in map {
                        let length = path.len();
                        path.push('/');
                        path.push_str(&key.replace('~', "~0").replace('/', "~1"));
                        walk(value, path, out);
                        path.truncate(length);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(self, &mut String::new(), &mut out);
        out
    }

    /// Rebuilds a tree from plain JSON. Strings at `plain_paths` stay in the
    /// clear; with no path list, strings that are timestamps under a key
    /// ending in `Date` do, which is where servers keep their only clear text.
    pub fn from_plain(value: &Value, plain_paths: Option<&[String]>) -> Self {
        fn walk(value: &Value, key: &str, path: &mut String, plain: Option<&[String]>) -> Node {
            match value {
                Value::Null => Node::Null,
                Value::Bool(b) => Node::Bool(*b),
                Value::Number(n) => Node::Number(n.clone()),
                Value::String(s) => {
                    let is_plain = match plain {
                        Some(paths) => paths.iter().any(|p| p == path),
                        None => key.ends_with("Date") && normalize_timestamp(s).is_some(),
                    };
                    if is_plain {
                        Node::Text(s.clone())
                    } else {
                        Node::Secret(s.clone())
                    }
                }
                Value::Array(items) => Node::List(
                    items
                        .iter()
                        .enumerate()
                        .map(|(index, item)| {
                            let length = path.len();
                            path.push_str(&format!("/{index}"));
                            let node = walk(item, key, path, plain);
                            path.truncate(length);
                            node
                        })
                        .collect(),
                ),
                Value::Object(map) => Node::Map(
                    map.iter()
                        .map(|(k, v)| {
                            let length = path.len();
                            path.push('/');
                            path.push_str(&k.replace('~', "~0").replace('/', "~1"));
                            let node = walk(v, k, path, plain);
                            path.truncate(length);
                            (k.clone(), node)
                        })
                        .collect(),
                ),
            }
        }
        walk(value, "", &mut String::new(), plain_paths)
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
    let parsed = OffsetDateTime::parse(value, &Rfc3339)
        .ok()?
        .to_offset(UtcOffset::UTC);
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

    #[test]
    fn diagnostic_output_redacts_nested_secrets() {
        let secret = "synthetic-vault-secret";
        let tree = Node::Map(BTreeMap::from([(
            "login".into(),
            Node::List(vec![
                Node::secret(secret),
                Node::Text("visible date".into()),
            ]),
        )]));
        let diagnostic = format!("{tree:?}");
        assert!(!diagnostic.contains(secret));
        assert!(diagnostic.contains("[redacted]"));
        assert!(diagnostic.contains("visible date"));
    }

    fn map(entries: Vec<(&str, Node)>) -> Node {
        Node::Map(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
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
            (
                "revisionDate",
                Node::Text("2026-01-02T03:04:05.000000Z".into()),
            ),
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
        let error = Node::decrypt(&raw, &SymKey::generate()).unwrap_err();
        assert_eq!(error.to_string(), "could not decrypt the value at /name");
    }

    #[test]
    fn ciphertext_in_an_unsupported_format_is_an_error_not_text() {
        let key = SymKey::generate();
        let enc = key.encrypt_str("hunter2").unwrap();
        // Type 1 is a legacy format that bitwarden-crypto does not read.
        let unsupported = format!("1{}", &enc[1..]);
        let raw = json!({
            "name": enc,
            "login": {"uris": [{"uri": unsupported, "match": 3}]},
        });
        let error = Node::decrypt(&raw, &key).unwrap_err();
        let message = error.to_string();
        assert_eq!(
            message,
            "the encrypted value at /login/uris/0/uri is malformed or in an unsupported format"
        );
        assert!(!message.contains(&unsupported[2..10]));
        assert_eq!(
            Node::decrypt(&json!(unsupported), &key)
                .unwrap_err()
                .to_string(),
            "the encrypted value is malformed or in an unsupported format"
        );
    }

    #[test]
    fn plain_values_that_servers_send_stay_text() {
        let key = SymKey::generate();
        let raw = json!({
            "login": {
                "passwordRevisionDate": "2026-01-02T03:04:05.1234567Z",
                "uris": [{"uri": key.encrypt_str("u").unwrap(), "match": 0}],
            },
            "passwordHistory": [{"lastUsedDate": "2025-01-01T00:00:00Z"}],
            "fido2Credentials": [{"creationDate": "2026-02-03T04:05:06.000Z"}],
            "version": "2.5",
            "reprompt": 0,
            "favorite": true,
            "notes": null,
        });
        let node = Node::decrypt(&raw, &key).unwrap();
        assert_eq!(
            node.plain_string_paths(),
            [
                "/fido2Credentials/0/creationDate",
                "/login/passwordRevisionDate",
                "/passwordHistory/0/lastUsedDate",
                "/version",
            ]
        );
    }

    #[test]
    fn plain_export_reveals_secrets() {
        let node = map(vec![
            ("name", Node::secret("n")),
            ("d", Node::Text("x".into())),
        ]);
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
            (
                "uris",
                Node::List(vec![Node::Null, map(vec![("uri", Node::secret("u"))])]),
            ),
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
            Node::Text("2026-01-02T03:04:05Z".into())
                .normalized()
                .unwrap(),
            json!("2026-01-02T03:04:05.000Z")
        );
    }

    #[test]
    fn a_plain_export_rebuilds_into_the_same_tree() {
        let node = map(vec![
            ("name", Node::secret("2026-01-02T03:04:05Z")),
            (
                "login",
                map(vec![
                    (
                        "passwordRevisionDate",
                        Node::Text("2026-01-02T03:04:05.000Z".into()),
                    ),
                    (
                        "uris",
                        Node::List(vec![map(vec![
                            ("uri", Node::secret("u")),
                            ("a/b~c", Node::Text("plain".into())),
                        ])]),
                    ),
                ]),
            ),
            ("n", Node::Number(1.into())),
        ]);
        let paths = node.plain_string_paths();
        assert_eq!(
            paths,
            ["/login/passwordRevisionDate", "/login/uris/0/a~1b~0c"]
        );
        assert_eq!(Node::from_plain(&node.to_plain(), Some(&paths)), node);
    }

    #[test]
    fn a_foreign_export_keeps_only_dated_timestamps_in_the_clear() {
        let plain = json!({
            "name": "2026-01-02T03:04:05Z",
            "login": {"passwordRevisionDate": "2026-01-02T03:04:05.000Z", "password": "pw"},
            "passwordHistory": [{"lastUsedDate": "2025-01-01T00:00:00Z", "password": "old"}],
            "expiryDate": "next week",
        });
        let node = Node::from_plain(&plain, None);
        let login = node.as_map().unwrap()["login"].as_map().unwrap();
        assert_eq!(
            login["passwordRevisionDate"],
            Node::Text("2026-01-02T03:04:05.000Z".into())
        );
        assert_eq!(login["password"], Node::secret("pw"));
        assert_eq!(
            node.as_map().unwrap()["name"],
            Node::secret("2026-01-02T03:04:05Z")
        );
        assert_eq!(
            node.as_map().unwrap()["expiryDate"],
            Node::secret("next week")
        );
        let history = node.as_map().unwrap()["passwordHistory"].as_list().unwrap()[0]
            .as_map()
            .unwrap();
        assert_eq!(
            history["lastUsedDate"],
            Node::Text("2025-01-01T00:00:00Z".into())
        );
    }

    #[test]
    fn only_plain_timestamps_are_rewritten() {
        // A secret that happens to look like a timestamp is user content.
        assert_eq!(
            Node::secret("2026-01-02T03:04:05Z").normalized().unwrap(),
            json!("2026-01-02T03:04:05Z")
        );
        for plain in [
            "not a date",
            "2026-01-02",
            "9a1c7c5e-0c1f-4a4b-9d39-0c1c2f7f7a10",
        ] {
            assert_eq!(Node::Text(plain.into()).normalized().unwrap(), json!(plain));
        }
    }
}
