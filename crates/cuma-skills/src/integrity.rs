//! What a skill's bytes prove.
//!
//! Two pieces of evidence, both computed here rather than read from the
//! manifest a skill ships with:
//!
//! - its **digest**: SHA-256 over every file in the skill directory, in a
//!   fixed order, so any change to any file changes it;
//! - its **signature**: an Ed25519 signature over that digest, checked
//!   against keys the operator configured under `[skills.trusted_keys]`.
//!
//! A manifest may *claim* a checksum or a signature; neither is evidence of
//! anything until it has been checked against the files themselves.
//!
//! ## `skill.sig`
//!
//! One line, `ed25519:<key-id>:<base64 signature>`, where the signed message
//! is the digest string (`sha256:<hex>`). The file is excluded from the digest
//! it signs.

use base64::Engine;
use cuma_core::error::{MetaAgentError, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The file holding a skill's signature.
pub const SIGNATURE_FILE: &str = "skill.sig";

/// The most files a skill may contain.
const MAX_FILES: usize = 1_000;

/// The most bytes a skill may contain.
const MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Every file in a skill directory, as sorted relative paths.
///
/// Refuses symlinks outright: a link inside a skill can point anywhere on the
/// machine, and following it would let a skill's digest — and its contents —
/// depend on files it does not contain.
pub fn skill_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![PathBuf::new()];
    let mut total: u64 = 0;

    while let Some(relative) = pending.pop() {
        let entries = std::fs::read_dir(root.join(&relative)).map_err(|err| {
            MetaAgentError::Skill(format!(
                "cannot read {}: {err}",
                root.join(&relative).display()
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|err| MetaAgentError::Skill(err.to_string()))?;
            let kind = entry
                .file_type()
                .map_err(|err| MetaAgentError::Skill(err.to_string()))?;
            let path = relative.join(entry.file_name());

            if kind.is_symlink() {
                return Err(MetaAgentError::Security(format!(
                    "skill contains a symbolic link at {}",
                    path.display()
                )));
            }
            if kind.is_dir() {
                if entry.file_name() != ".git" {
                    pending.push(path);
                }
                continue;
            }
            if path == Path::new(SIGNATURE_FILE) {
                continue;
            }

            total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
            files.push(path);
            if files.len() > MAX_FILES || total > MAX_BYTES {
                return Err(MetaAgentError::Security(format!(
                    "skill exceeds {MAX_FILES} files or {MAX_BYTES} bytes"
                )));
            }
        }
    }

    files.sort();
    Ok(files)
}

/// The digest of a skill directory: `sha256:<hex>`.
///
/// Each file contributes its relative path (with `/` separators), a NUL, its
/// length and its bytes, so renaming a file, moving bytes between files or
/// adding an empty file all change the digest.
pub fn content_digest(root: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    for relative in skill_files(root)? {
        let bytes = std::fs::read(root.join(&relative)).map_err(|err| {
            MetaAgentError::Skill(format!("cannot read {}: {err}", relative.display()))
        })?;
        let name = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        hasher.update(name.as_bytes());
        hasher.update([0u8]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(format!("sha256:{}", to_hex(&hasher.finalize())))
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Keys whose signatures make a skill `Trusted`.
#[derive(Debug, Clone, Default)]
pub struct TrustedKeys {
    keys: BTreeMap<String, VerifyingKey>,
}

impl TrustedKeys {
    /// Parse `name → base64 public key` pairs, as configured.
    ///
    /// A key that does not parse is an error rather than skipped: an operator
    /// who believes a key is trusted and has mistyped it should find out now,
    /// not when every skill signed with it is quietly demoted.
    pub fn from_config(configured: &BTreeMap<String, String>) -> Result<Self> {
        let mut keys = BTreeMap::new();
        for (name, encoded) in configured {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .map_err(|err| {
                    MetaAgentError::Configuration(format!(
                        "skills.trusted_keys.{name}: not base64: {err}"
                    ))
                })?;
            let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
                MetaAgentError::Configuration(format!(
                    "skills.trusted_keys.{name}: an Ed25519 public key is 32 bytes"
                ))
            })?;
            let key = VerifyingKey::from_bytes(&bytes).map_err(|err| {
                MetaAgentError::Configuration(format!("skills.trusted_keys.{name}: {err}"))
            })?;
            keys.insert(name.clone(), key);
        }
        Ok(Self { keys })
    }

    /// Whether any key is configured.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// What a skill's signature file showed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureCheck {
    /// There is no signature.
    Absent,
    /// Signed by a configured key, and the signature holds.
    Valid {
        /// The key's configured name.
        key: String,
    },
    /// Signed by a key the operator has not configured.
    UnknownKey {
        /// The key id the signature names.
        key: String,
    },
    /// The signature does not verify, or cannot be read.
    Invalid {
        /// Why.
        reason: String,
    },
}

/// Check `signature_line` — the contents of `skill.sig` — against `digest`.
pub fn check_signature(digest: &str, signature_line: &str, keys: &TrustedKeys) -> SignatureCheck {
    let line = signature_line.trim();
    if line.is_empty() {
        return SignatureCheck::Absent;
    }

    let mut parts = line.splitn(3, ':');
    let (Some("ed25519"), Some(key_id), Some(encoded)) = (parts.next(), parts.next(), parts.next())
    else {
        return SignatureCheck::Invalid {
            reason: "skill.sig is not `ed25519:<key-id>:<base64>`".to_owned(),
        };
    };

    let Some(key) = keys.keys.get(key_id) else {
        return SignatureCheck::UnknownKey {
            key: key_id.to_owned(),
        };
    };

    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
        return SignatureCheck::Invalid {
            reason: "the signature is not base64".to_owned(),
        };
    };
    let Ok(bytes) = <[u8; 64]>::try_from(bytes) else {
        return SignatureCheck::Invalid {
            reason: "an Ed25519 signature is 64 bytes".to_owned(),
        };
    };

    // `verify_strict` rejects the malleable and small-order edge cases plain
    // `verify` tolerates.
    match key.verify_strict(digest.as_bytes(), &Signature::from_bytes(&bytes)) {
        Ok(()) => SignatureCheck::Valid {
            key: key_id.to_owned(),
        },
        Err(_) => SignatureCheck::Invalid {
            reason: format!("the signature by {key_id:?} does not match the skill's contents"),
        },
    }
}

/// Produce a `skill.sig` line for `digest` — the publisher's side.
pub fn sign(digest: &str, key_id: &str, key: &SigningKey) -> String {
    let signature = key.sign(digest.as_bytes());
    format!(
        "ed25519:{key_id}:{}",
        base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
    )
}

/// The base64 public key for `key`, as `[skills.trusted_keys]` expects it.
pub fn public_key(key: &SigningKey) -> String {
    base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes())
}

/// What installing a skill's files established.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Evidence {
    /// The digest computed from the files, when there are files.
    pub digest: Option<String>,
    /// The digest the registry published for this skill, when it did.
    pub published_digest: Option<String>,
    /// What the signature showed.
    pub signature: Option<SignatureCheck>,
}

impl Evidence {
    /// Examine the skill in `root`.
    pub fn gather(
        root: &Path,
        published_digest: Option<String>,
        keys: &TrustedKeys,
    ) -> Result<Self> {
        let digest = content_digest(root)?;
        let signature_line = std::fs::read_to_string(root.join(SIGNATURE_FILE)).unwrap_or_default();
        let signature = check_signature(&digest, &signature_line, keys);
        Ok(Self {
            digest: Some(digest),
            published_digest,
            signature: Some(signature),
        })
    }

    /// Whether the computed digest matches the one published.
    pub fn digest_matches(&self) -> Option<bool> {
        Some(self.digest.as_ref()? == self.published_digest.as_ref()?)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn skill(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, contents) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, contents).unwrap();
        }
        dir
    }

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn keys() -> TrustedKeys {
        TrustedKeys::from_config(&BTreeMap::from([("acme".to_owned(), public_key(&key()))]))
            .unwrap()
    }

    #[test]
    fn the_digest_is_stable_and_covers_every_byte_and_name() {
        let a = skill(&[("skill.toml", "id = 'x'"), ("SKILL.md", "do things")]);
        let b = skill(&[("SKILL.md", "do things"), ("skill.toml", "id = 'x'")]);
        assert_eq!(
            content_digest(a.path()).unwrap(),
            content_digest(b.path()).unwrap()
        );

        let changed = skill(&[("skill.toml", "id = 'x'"), ("SKILL.md", "do other things")]);
        let renamed = skill(&[("skill.toml", "id = 'x'"), ("GUIDE.md", "do things")]);
        let digest = content_digest(a.path()).unwrap();
        assert_ne!(digest, content_digest(changed.path()).unwrap());
        assert_ne!(digest, content_digest(renamed.path()).unwrap());
        assert!(digest.starts_with("sha256:") && digest.len() == 7 + 64);
    }

    #[test]
    fn the_signature_file_is_not_part_of_what_it_signs() {
        let dir = skill(&[("skill.toml", "id = 'x'")]);
        let before = content_digest(dir.path()).unwrap();
        std::fs::write(dir.path().join(SIGNATURE_FILE), "anything").unwrap();
        assert_eq!(content_digest(dir.path()).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_a_skill_is_refused() {
        let dir = skill(&[("skill.toml", "id = 'x'")]);
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("passwd")).unwrap();
        assert!(content_digest(dir.path()).is_err());
    }

    #[test]
    fn a_signature_by_a_configured_key_is_valid() {
        let dir = skill(&[("skill.toml", "id = 'x'"), ("SKILL.md", "guide")]);
        let digest = content_digest(dir.path()).unwrap();
        let line = sign(&digest, "acme", &key());
        assert_eq!(
            check_signature(&digest, &line, &keys()),
            SignatureCheck::Valid { key: "acme".into() }
        );
    }

    #[test]
    fn a_signature_over_different_contents_is_invalid() {
        let line = sign("sha256:00", "acme", &key());
        assert!(matches!(
            check_signature("sha256:01", &line, &keys()),
            SignatureCheck::Invalid { .. }
        ));
    }

    #[test]
    fn a_signature_by_an_unconfigured_key_proves_nothing() {
        let stranger = SigningKey::from_bytes(&[9u8; 32]);
        let line = sign("sha256:00", "stranger", &stranger);
        assert_eq!(
            check_signature("sha256:00", &line, &keys()),
            SignatureCheck::UnknownKey {
                key: "stranger".into()
            }
        );
    }

    #[test]
    fn a_key_claiming_a_configured_name_but_signing_with_another_key_is_invalid() {
        let impostor = SigningKey::from_bytes(&[9u8; 32]);
        let line = sign("sha256:00", "acme", &impostor);
        assert!(matches!(
            check_signature("sha256:00", &line, &keys()),
            SignatureCheck::Invalid { .. }
        ));
    }

    #[test]
    fn a_mistyped_trusted_key_is_a_configuration_error() {
        let configured = BTreeMap::from([("acme".to_owned(), "not-a-key".to_owned())]);
        assert!(TrustedKeys::from_config(&configured).is_err());
    }

    #[test]
    fn malformed_signature_lines_are_invalid_not_absent() {
        for line in ["rsa:acme:xyz", "ed25519:acme", "ed25519:acme:!!!"] {
            assert!(
                matches!(
                    check_signature("sha256:00", line, &keys()),
                    SignatureCheck::Invalid { .. }
                ),
                "{line}"
            );
        }
        assert_eq!(
            check_signature("sha256:00", "  ", &keys()),
            SignatureCheck::Absent
        );
    }
}
