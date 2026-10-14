//! The only module that talks to `bitwarden-crypto`.
//!
//! Everything else works with [`SymKey`] and plain strings, so a change in the
//! upstream crate's API stays contained here.

use std::num::NonZeroU32;

use bitwarden_crypto::{
    BitwardenLegacyKeyBytes, CoseKeyBytes, EncString, EncodedSymmetricKey, HashPurpose, Kdf,
    KeyDecryptable, KeyEncryptable, MasterKey, OctetStreamBytes, Pkcs8PrivateKeyBytes, PrivateKey,
    SymmetricCryptoKey, SymmetricKeyAlgorithm, UnsignedSharedKey, UserKey,
};

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("the master password is wrong, or the account uses an unsupported key format")]
    WrongMasterPassword,
    #[error("unsupported key derivation settings: {0}")]
    UnsupportedKdf(String),
    #[error("could not decrypt {what}")]
    Decrypt { what: &'static str },
    #[error("could not encrypt {what}")]
    Encrypt { what: &'static str },
    #[error("malformed encrypted value for {what}")]
    Malformed { what: &'static str },
}

type Result<T> = std::result::Result<T, CryptoError>;

/// Key derivation parameters as the server reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KdfParams {
    Pbkdf2 {
        iterations: u32,
    },
    Argon2id {
        iterations: u32,
        memory_mib: u32,
        parallelism: u32,
    },
}

impl KdfParams {
    pub fn from_server(
        kdf: u64,
        iterations: u64,
        memory: Option<u64>,
        parallelism: Option<u64>,
    ) -> Result<Self> {
        let narrow = |value: u64, name: &str| {
            u32::try_from(value)
                .ok()
                .filter(|v| *v > 0)
                .ok_or_else(|| CryptoError::UnsupportedKdf(format!("{name}={value}")))
        };
        match kdf {
            0 => Ok(Self::Pbkdf2 {
                iterations: narrow(iterations, "iterations")?,
            }),
            1 => Ok(Self::Argon2id {
                iterations: narrow(iterations, "iterations")?,
                memory_mib: narrow(memory.unwrap_or(0), "memory")?,
                parallelism: narrow(parallelism.unwrap_or(0), "parallelism")?,
            }),
            other => Err(CryptoError::UnsupportedKdf(format!("kdf type {other}"))),
        }
    }

    fn to_upstream(&self) -> Kdf {
        // `from_server` rejects zero, and every other constructor uses literals.
        let nz = |v: u32| NonZeroU32::new(v).expect("kdf parameter is non-zero");
        match *self {
            Self::Pbkdf2 { iterations } => Kdf::PBKDF2 {
                iterations: nz(iterations),
            },
            Self::Argon2id {
                iterations,
                memory_mib,
                parallelism,
            } => Kdf::Argon2id {
                iterations: nz(iterations),
                memory: nz(memory_mib),
                parallelism: nz(parallelism),
            },
        }
    }
}

/// Returns true when `value` is a well-formed Bitwarden encrypted string.
///
/// The vault model is decrypted without a schema, so this decides which JSON
/// strings are ciphertext. Plain values the server stores next to ciphertext
/// are ids, dates, numbers and booleans, none of which parse as one. A value
/// that passes here and then fails to decrypt is an error, never plaintext.
pub fn looks_encrypted(value: &str) -> bool {
    // Cheap reject before the full parse: "<digits>.<base64 parts>".
    let Some((kind, rest)) = value.split_once('.') else {
        return false;
    };
    if kind.is_empty() || !kind.bytes().all(|b| b.is_ascii_digit()) || rest.len() < 16 {
        return false;
    }
    EncString::parse_strict(value).is_ok()
}

/// A symmetric vault key: a user key, organization key, item key or attachment key.
pub struct SymKey(SymmetricCryptoKey);

impl SymKey {
    pub fn generate() -> Self {
        Self(SymmetricCryptoKey::make(
            SymmetricKeyAlgorithm::Aes256CbcHmac,
        ))
    }

    pub fn decrypt_str(&self, value: &str) -> Result<String> {
        let enc = parse(value, "text")?;
        enc.decrypt_with_key(&self.0)
            .map_err(|_| CryptoError::Decrypt { what: "text" })
    }

    pub fn encrypt_str(&self, value: &str) -> Result<String> {
        value
            .encrypt_with_key(&self.0)
            .map(|enc| enc.to_string())
            .map_err(|_| CryptoError::Encrypt { what: "text" })
    }

    /// Decrypts a key that was wrapped with this key.
    pub fn unwrap_key(&self, wrapped: &str) -> Result<SymKey> {
        let enc = parse(wrapped, "wrapped key")?;
        let bytes: Vec<u8> = enc
            .decrypt_with_key(&self.0)
            .map_err(|_| CryptoError::Decrypt {
                what: "wrapped key",
            })?;
        key_from_bytes(bytes).ok_or(CryptoError::Decrypt {
            what: "wrapped key",
        })
    }

    pub fn wrap_key(&self, key: &SymKey) -> Result<String> {
        let encoded = key.0.to_encoded();
        (&encoded)
            .encrypt_with_key(&self.0)
            .map(|enc| enc.to_string())
            .map_err(|_| CryptoError::Encrypt {
                what: "wrapped key",
            })
    }

    /// Decrypts a file in the binary attachment format.
    pub fn decrypt_blob(&self, blob: &[u8]) -> Result<Vec<u8>> {
        let enc = EncString::from_buffer(blob)
            .map_err(|_| CryptoError::Malformed { what: "attachment" })?;
        enc.decrypt_with_key(&self.0)
            .map_err(|_| CryptoError::Decrypt { what: "attachment" })
    }

    pub fn encrypt_blob(&self, data: &[u8]) -> Result<Vec<u8>> {
        let failed = |_| CryptoError::Encrypt { what: "attachment" };
        (&OctetStreamBytes::from(data))
            .encrypt_with_key(&self.0)
            .map_err(failed)?
            .to_buffer()
            .map_err(failed)
    }

    /// Decrypts the account's RSA private key, which is stored wrapped with the user key.
    pub fn unwrap_private_key(&self, wrapped: &str) -> Result<AccountPrivateKey> {
        let what = "account private key";
        let enc = parse(wrapped, what)?;
        let der: Vec<u8> = enc
            .decrypt_with_key(&self.0)
            .map_err(|_| CryptoError::Decrypt { what })?;
        PrivateKey::from_der(&Pkcs8PrivateKeyBytes::from(der))
            .map(AccountPrivateKey)
            .map_err(|_| CryptoError::Decrypt { what })
    }
}

fn parse(value: &str, what: &'static str) -> Result<EncString> {
    EncString::parse_strict(value).map_err(|_| CryptoError::Malformed { what })
}

fn key_from_bytes(bytes: Vec<u8>) -> Option<SymKey> {
    if let Ok(key) = SymmetricCryptoKey::try_from(&BitwardenLegacyKeyBytes::from(bytes.clone())) {
        return Some(SymKey(key));
    }
    // Accounts on the newer encryption scheme can wrap keys as bare COSE keys.
    SymmetricCryptoKey::try_from(EncodedSymmetricKey::CoseKey(CoseKeyBytes::from(bytes)))
        .ok()
        .map(SymKey)
}

/// The account's RSA private key. Organization keys are shared to it.
pub struct AccountPrivateKey(PrivateKey);

impl AccountPrivateKey {
    pub fn unwrap_shared_key(&self, wrapped: &str) -> Result<SymKey> {
        let what = "organization key";
        let shared: UnsignedSharedKey = wrapped
            .parse()
            .map_err(|_| CryptoError::Malformed { what })?;
        #[expect(
            deprecated,
            reason = "the replacement needs a key store this tool has no use for"
        )]
        shared
            .decapsulate_key_unsigned(&self.0)
            .map(SymKey)
            .map_err(|_| CryptoError::Decrypt { what })
    }
}

/// The key derived from the master password. It only unlocks the user key.
pub struct MasterSecret(MasterKey);

impl MasterSecret {
    /// `salt` is the account email; the server treats it case-insensitively.
    pub fn derive(password: &str, salt: &str, kdf: &KdfParams) -> Result<Self> {
        let salt = salt.trim().to_lowercase();
        MasterKey::derive(password, &salt, &kdf.to_upstream())
            .map(Self)
            .map_err(|err| CryptoError::UnsupportedKdf(err.to_string()))
    }

    pub fn unlock_user_key(&self, wrapped: &str) -> Result<SymKey> {
        let enc = parse(wrapped, "user key")?;
        self.0
            .decrypt_user_key(enc)
            .map(SymKey)
            .map_err(|_| CryptoError::WrongMasterPassword)
    }

    /// The hash the server stores to authenticate password-based requests.
    pub fn server_hash(&self, password: &str) -> String {
        self.0
            .derive_master_key_hash(password.as_bytes(), HashPurpose::ServerAuthorization)
            .to_string()
    }

    /// Creates the key material a brand-new account registers with.
    pub fn new_account_keys(&self) -> Result<NewAccountKeys> {
        let failed = |_| CryptoError::Encrypt {
            what: "new account keys",
        };
        let (user_key, wrapped_user_key) = self.0.make_user_key().map_err(failed)?;
        let pair = user_key.make_key_pair().map_err(failed)?;
        let UserKey(user_key) = user_key;
        Ok(NewAccountKeys {
            user_key: SymKey(user_key),
            wrapped_user_key: wrapped_user_key.to_string(),
            public_key: pair.public.to_string(),
            wrapped_private_key: pair.private.to_string(),
        })
    }
}

pub struct NewAccountKeys {
    pub user_key: SymKey,
    pub wrapped_user_key: String,
    pub public_key: String,
    pub wrapped_private_key: String,
}

/// Wraps `key` for the holder of the RSA public key `public_key_b64`.
pub fn share_key_with(public_key_b64: &str, key: &SymKey) -> Result<String> {
    use base64::Engine;
    use bitwarden_crypto::{PublicKey, SpkiPublicKeyBytes};

    let what = "shared key";
    let der = base64::engine::general_purpose::STANDARD
        .decode(public_key_b64)
        .map_err(|_| CryptoError::Malformed { what })?;
    let public = PublicKey::from_der(&SpkiPublicKeyBytes::from(der))
        .map_err(|_| CryptoError::Malformed { what })?;
    #[expect(
        deprecated,
        reason = "the replacement needs a key store this tool has no use for"
    )]
    UnsignedSharedKey::encapsulate_key_unsigned(&key.0, &public)
        .map(|shared| shared.to_string())
        .map_err(|_| CryptoError::Encrypt { what })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST_KDF: KdfParams = KdfParams::Pbkdf2 { iterations: 5000 };

    #[test]
    fn text_round_trips_and_is_recognised_as_ciphertext() {
        let key = SymKey::generate();
        let enc = key.encrypt_str("hunter2 ü 🔑").unwrap();
        assert!(looks_encrypted(&enc), "{enc}");
        assert_eq!(key.decrypt_str(&enc).unwrap(), "hunter2 ü 🔑");
    }

    #[test]
    fn a_different_key_cannot_decrypt() {
        let enc = SymKey::generate().encrypt_str("secret").unwrap();
        assert!(SymKey::generate().decrypt_str(&enc).is_err());
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let key = SymKey::generate();
        let enc = key.encrypt_str("secret").unwrap();
        let mut tampered = enc.clone().into_bytes();
        let index = enc.find('|').unwrap() + 3;
        tampered[index] = if tampered[index] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(key.decrypt_str(&tampered).is_err());
    }

    #[test]
    fn plain_values_are_not_mistaken_for_ciphertext() {
        for plain in [
            "",
            "example.com",
            "2026-10-14T00:00:00.000000Z",
            "1.5",
            "2.",
            "9a1c7c5e-0c1f-4a4b-9d39-0c1c2f7f7a10",
            "v2.local",
            "2.has space|x|y",
            "7.1234",
            "2.AAAAAAAAAAAAAAAAAAAAAA==|AAAA",
        ] {
            assert!(!looks_encrypted(plain), "{plain}");
        }
    }

    #[test]
    fn wrapped_keys_round_trip() {
        let outer = SymKey::generate();
        let inner = SymKey::generate();
        let wrapped = outer.wrap_key(&inner).unwrap();
        let unwrapped = outer.unwrap_key(&wrapped).unwrap();
        let enc = inner.encrypt_str("via item key").unwrap();
        assert_eq!(unwrapped.decrypt_str(&enc).unwrap(), "via item key");
    }

    #[test]
    fn attachment_blobs_round_trip() {
        let key = SymKey::generate();
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let blob = key.encrypt_blob(&data).unwrap();
        assert_ne!(blob, data);
        assert_eq!(key.decrypt_blob(&blob).unwrap(), data);
        assert!(SymKey::generate().decrypt_blob(&blob).is_err());
    }

    #[test]
    fn master_password_unlocks_a_new_account() {
        let master = MasterSecret::derive("correct horse", "User@Example.com", &FAST_KDF).unwrap();
        let account = master.new_account_keys().unwrap();
        let enc = account.user_key.encrypt_str("vault item").unwrap();

        // The salt is case-insensitive, as it is on the server.
        let again = MasterSecret::derive("correct horse", "user@example.com", &FAST_KDF).unwrap();
        let user_key = again.unlock_user_key(&account.wrapped_user_key).unwrap();
        assert_eq!(user_key.decrypt_str(&enc).unwrap(), "vault item");
        assert_eq!(
            master.server_hash("correct horse"),
            again.server_hash("correct horse")
        );

        let wrong = MasterSecret::derive("wrong horse", "user@example.com", &FAST_KDF).unwrap();
        assert!(matches!(
            wrong.unlock_user_key(&account.wrapped_user_key),
            Err(CryptoError::WrongMasterPassword)
        ));
    }

    #[test]
    fn organization_keys_are_shared_through_the_account_key_pair() {
        let master = MasterSecret::derive("pw", "a@b.c", &FAST_KDF).unwrap();
        let account = master.new_account_keys().unwrap();
        let org_key = SymKey::generate();
        let shared = share_key_with(&account.public_key, &org_key).unwrap();

        let private = account
            .user_key
            .unwrap_private_key(&account.wrapped_private_key)
            .unwrap();
        let received = private.unwrap_shared_key(&shared).unwrap();
        let enc = org_key.encrypt_str("collection name").unwrap();
        assert_eq!(received.decrypt_str(&enc).unwrap(), "collection name");
    }

    #[test]
    fn argon2_accounts_derive() {
        let kdf = KdfParams::from_server(1, 2, Some(16), Some(1)).unwrap();
        let master = MasterSecret::derive("pw", "a@b.c", &kdf).unwrap();
        let account = master.new_account_keys().unwrap();
        assert!(master.unlock_user_key(&account.wrapped_user_key).is_ok());
    }

    #[test]
    fn invalid_kdf_settings_are_rejected() {
        assert!(KdfParams::from_server(0, 0, None, None).is_err());
        assert!(KdfParams::from_server(1, 3, None, Some(4)).is_err());
        assert!(KdfParams::from_server(7, 3, None, None).is_err());
    }
}
