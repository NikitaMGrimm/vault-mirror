//! A sample vault that exercises every item type and awkward value the
//! mirror has to carry. Used by the integration suite and the sandbox command.

use std::collections::BTreeMap;

use crate::node::Node;

pub struct Sample {
    pub folder: Option<&'static str>,
    pub content: BTreeMap<String, Node>,
    pub attachments: Vec<(&'static str, Vec<u8>)>,
    pub archived: bool,
}

fn secret(value: &str) -> Node {
    Node::secret(value)
}

fn number(value: u64) -> Node {
    Node::Number(value.into())
}

fn map(entries: Vec<(&str, Node)>) -> Node {
    Node::Map(entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn item(kind: u64, name: &str, extra: Vec<(&str, Node)>) -> BTreeMap<String, Node> {
    let mut content = BTreeMap::from([
        ("type".to_string(), number(kind)),
        ("name".to_string(), secret(name)),
        ("notes".to_string(), Node::Null),
        ("favorite".to_string(), Node::Bool(false)),
        ("reprompt".to_string(), number(0)),
    ]);
    for (key, value) in extra {
        content.insert(key.to_string(), value);
    }
    content
}

fn field(name: &str, value: Node, kind: u64, linked: Node) -> Node {
    map(vec![("name", secret(name)), ("value", value), ("type", number(kind)), ("linkedId", linked)])
}

fn sample(folder: Option<&'static str>, content: BTreeMap<String, Node>) -> Sample {
    Sample {
        folder,
        content,
        attachments: Vec::new(),
        archived: false,
    }
}

/// Folder names the sample vault uses, including one that stays empty.
pub const FOLDERS: &[&str] = &["Work", "Work/Infra", "Persönlich ✈", "Empty"];

pub fn samples() -> Vec<Sample> {
    let uri = |value: &str, matching: Node| map(vec![("uri", secret(value)), ("match", matching)]);
    let mut samples = vec![
        sample(
            Some("Work"),
            item(
                1,
                "GitHub",
                vec![
                    ("favorite", Node::Bool(true)),
                    ("notes", secret("Recovery codes:\n  1234-5678\n  abcd-efgh\n")),
                    (
                        "login",
                        map(vec![
                            ("username", secret("octocat")),
                            ("password", secret("correct horse battery staple")),
                            ("totp", secret("otpauth://totp/GitHub:octocat?secret=JBSWY3DPEHPK3PXP&issuer=GitHub")),
                            ("passwordRevisionDate", Node::Text("2026-03-04T05:06:07.000Z".into())),
                            (
                                "uris",
                                Node::List(vec![
                                    uri("https://github.com/login", Node::Null),
                                    uri("https://gist.github.com", number(1)),
                                    uri("androidapp://com.github.android", number(3)),
                                ]),
                            ),
                        ]),
                    ),
                    (
                        "fields",
                        Node::List(vec![
                            field("text field", secret("visible"), 0, Node::Null),
                            field("hidden field", secret("s3cr3t"), 1, Node::Null),
                            field("boolean field", secret("true"), 2, Node::Null),
                            field("linked field", Node::Null, 3, number(100)),
                        ]),
                    ),
                    (
                        "passwordHistory",
                        Node::List(vec![
                            map(vec![("password", secret("old-password-1")), ("lastUsedDate", Node::Text("2025-01-02T03:04:05.000Z".into()))]),
                            map(vec![("password", secret("old-password-2")), ("lastUsedDate", Node::Text("2025-06-07T08:09:10.000Z".into()))]),
                        ]),
                    ),
                ],
            ),
        ),
        sample(
            Some("Work/Infra"),
            item(
                1,
                "Passkey login",
                vec![(
                    "login",
                    map(vec![
                        ("username", secret("passkey-user")),
                        ("password", Node::Null),
                        ("totp", Node::Null),
                        ("uris", Node::List(vec![uri("https://webauthn.io", Node::Null)])),
                        (
                            "fido2Credentials",
                            Node::List(vec![map(vec![
                                ("credentialId", secret("b64.credential-id")),
                                ("keyType", secret("public-key")),
                                ("keyAlgorithm", secret("ECDSA")),
                                ("keyCurve", secret("P-256")),
                                ("keyValue", secret("MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg")),
                                ("rpId", secret("webauthn.io")),
                                ("rpName", secret("WebAuthn.io")),
                                ("userHandle", secret("dXNlci1oYW5kbGU")),
                                ("userName", secret("passkey-user")),
                                ("userDisplayName", secret("Passkey User")),
                                ("counter", secret("0")),
                                ("discoverable", secret("true")),
                                ("creationDate", Node::Text("2026-02-03T04:05:06.000Z".into())),
                            ])]),
                        ),
                    ]),
                )],
            ),
        ),
        sample(
            None,
            item(
                1,
                "Bare login without folder",
                vec![("login", map(vec![("username", Node::Null), ("password", secret("only a password")), ("totp", Node::Null)]))],
            ),
        ),
        sample(
            Some("Persönlich ✈"),
            item(
                2,
                "Sichere Notiz — 日本語 🔐",
                vec![
                    ("notes", secret(&"Zeile mit Umlauten äöüß und Emoji 🎉\n".repeat(100))),
                    ("secureNote", map(vec![("type", number(0))])),
                    ("reprompt", number(1)),
                ],
            ),
        ),
        sample(
            Some("Persönlich ✈"),
            item(
                3,
                "Visa",
                vec![(
                    "card",
                    map(vec![
                        ("cardholderName", secret("Jane Doe")),
                        ("brand", secret("Visa")),
                        ("number", secret("4111111111111111")),
                        ("expMonth", secret("12")),
                        ("expYear", secret("2031")),
                        ("code", secret("123")),
                    ]),
                )],
            ),
        ),
        sample(
            None,
            item(
                4,
                "Identity",
                vec![(
                    "identity",
                    map(vec![
                        ("title", secret("Dr")),
                        ("firstName", secret("Jane")),
                        ("middleName", Node::Null),
                        ("lastName", secret("Doe")),
                        ("address1", secret("1 Main St")),
                        ("address2", Node::Null),
                        ("address3", Node::Null),
                        ("city", secret("Springfield")),
                        ("state", secret("IL")),
                        ("postalCode", secret("62701")),
                        ("country", secret("US")),
                        ("company", secret("ACME")),
                        ("email", secret("jane@example.com")),
                        ("phone", secret("+1 555 0100")),
                        ("ssn", secret("000-00-0000")),
                        ("username", secret("jdoe")),
                        ("passportNumber", secret("X1234567")),
                        ("licenseNumber", secret("D1234567")),
                    ]),
                )],
            ),
        ),
        sample(
            Some("Work/Infra"),
            item(
                5,
                "Deploy key",
                vec![(
                    "sshKey",
                    map(vec![
                        ("privateKey", secret("-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\n")),
                        ("publicKey", secret("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample deploy@example")),
                        ("keyFingerprint", secret("SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG")),
                    ]),
                )],
            ),
        ),
        sample(Some("Work"), item(2, "Archived note", vec![("secureNote", map(vec![("type", number(0))])), ("notes", secret("kept but archived"))])),
        sample(
            None,
            item(
                1,
                "Looks like ciphertext",
                // Plaintext that has the shape of an encrypted string must survive as-is.
                vec![
                    ("notes", secret("2.AAAAAAAAAAAAAAAAAAAAAA==|AAAAAAAAAAAAAAAAAAAAAA==|AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")),
                    ("login", map(vec![("username", secret("")), ("password", secret("  spaces  ")), ("totp", Node::Null)])),
                ],
            ),
        ),
    ];
    samples[0].attachments = vec![
        ("recovery codes.txt", b"1234-5678\nabcd-efgh\n".to_vec()),
        ("binary.bin", (0..70_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect()),
    ];
    samples[3].attachments = vec![("empty.txt", Vec::new()), ("ünïcödé ✈.txt", "größe".as_bytes().to_vec())];
    samples[7].archived = true;
    samples
}
