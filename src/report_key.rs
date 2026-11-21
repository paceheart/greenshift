//! Validated identities for captured report imports, independent of file paths.

use sha2::{Digest, Sha256};

mod payload;
pub use payload::{ReportKey, ReportKeySet};

/// A commit id as `ingest --commit` takes it: 7 to 64 hex digits, a full
/// SHA-1 (40) or SHA-256 (64) object name or an abbreviation of one at
/// least as long as git's default. Upper-case digits are accepted and kept
/// in lower case, as git writes them, so one commit is stored one way.
///
/// Only new input is checked: `runs.commit_sha` has no constraint, and
/// rows written before this check may hold anything. Nothing reads them
/// back as a `CommitId`, so they are never rejected.
/// Abbreviations and full object names remain distinct; no Git lookup occurs.
///
/// ```
/// let id: greenshift::store::CommitId = "0123ABCD".parse().unwrap();
/// assert_eq!(id.as_str(), "0123abcd");
/// assert!("HEAD".parse::<greenshift::store::CommitId>().is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommitId(String);

/// Why text is not a [`CommitId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a commit is {min} to {max} hex digits", min = CommitId::MIN_LEN, max = CommitId::MAX_LEN)]
pub struct CommitIdError;

impl CommitId {
    /// The fewest digits taken: git's default abbreviation.
    pub const MIN_LEN: usize = 7;
    /// The most digits taken: a full SHA-256 object name.
    pub const MAX_LEN: usize = 64;

    /// The id in lower case, as stored.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for CommitId {
    type Err = CommitIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if (Self::MIN_LEN..=Self::MAX_LEN).contains(&text.len())
            && text.bytes().all(|b| b.is_ascii_hexdigit())
        {
            Ok(Self(text.to_ascii_lowercase()))
        } else {
            Err(CommitIdError)
        }
    }
}

impl std::fmt::Display for CommitId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Invalid persisted key data or an invalid import key set.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReportKeyError {
    #[error("report commit namespace must be empty or 7 to 64 lowercase hex digits")]
    Namespace,
    #[error("unsupported report fingerprint version")]
    Version,
    #[error("report digest must be 64 lowercase hex digits")]
    Digest,
    #[error("a report import requires at least one report key")]
    Empty,
    #[error("a report import must use one commit namespace")]
    MixedNamespaces,
}

impl ReportKey {
    /// Hash the same bytes supplied to the report parser, without reopening a path.
    pub fn from_bytes(commit: Option<&CommitId>, bytes: &[u8]) -> Self {
        Self::from_validated_parts(
            commit.map_or("", CommitId::as_str).to_owned(),
            format!("{:x}", Sha256::digest(bytes)),
        )
    }

    /// Admit transported key data. Persisted spellings must already be canonical.
    pub fn new(namespace: &str, version: i64, digest: &str) -> Result<Self, ReportKeyError> {
        if !namespace.is_empty()
            && (!(CommitId::MIN_LEN..=CommitId::MAX_LEN).contains(&namespace.len())
                || !lower_hex(namespace))
        {
            return Err(ReportKeyError::Namespace);
        }
        if version != Self::VERSION {
            return Err(ReportKeyError::Version);
        }
        if digest.len() != 64 || !lower_hex(digest) {
            return Err(ReportKeyError::Digest);
        }
        Ok(Self::from_validated_parts(
            namespace.to_owned(),
            digest.to_owned(),
        ))
    }
}

fn lower_hex(text: &str) -> bool {
    text.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl ReportKeySet {
    pub fn new(keys: impl IntoIterator<Item = ReportKey>) -> Result<Self, ReportKeyError> {
        let mut keys: Vec<_> = keys.into_iter().collect();
        keys.sort_unstable();
        keys.dedup();
        let first = keys.first().ok_or(ReportKeyError::Empty)?;
        if keys.iter().any(|key| key.namespace() != first.namespace()) {
            return Err(ReportKeyError::MixedNamespaces);
        }
        Ok(Self::from_canonical(keys))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_report_key_raw_bytes_and_textual_commit_identity() {
        let commit: CommitId = "0123ABCD".parse().unwrap();
        let key = ReportKey::from_bytes(Some(&commit), b"abc");
        assert_eq!(key.namespace(), "0123abcd");
        assert_eq!(
            key.digest(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_ne!(key, ReportKey::from_bytes(None, b"abc"));
        assert_ne!(key, ReportKey::from_bytes(Some(&commit), b"abc\n"));
        let full: CommitId = "0123abcd00000000000000000000000000000000".parse().unwrap();
        assert_ne!(key, ReportKey::from_bytes(Some(&full), b"abc"));
    }

    #[test]
    fn test_report_key_new_rejects_noncanonical_transport_data() {
        let digest = "a".repeat(64);
        for namespace in ["ABCDEF0", "abcdef", "g123456", "é123456"] {
            assert_eq!(
                ReportKey::new(namespace, 1, &digest),
                Err(ReportKeyError::Namespace)
            );
        }
        assert!(ReportKey::new("", 1, &digest).is_ok());
        assert!(ReportKey::new(&"f".repeat(64), 1, &digest).is_ok());
        assert_eq!(ReportKey::new("", 2, &digest), Err(ReportKeyError::Version));
        for invalid in ["A".repeat(64), "a".repeat(63), "g".repeat(64)] {
            assert_eq!(ReportKey::new("", 1, &invalid), Err(ReportKeyError::Digest));
        }
    }

    #[test]
    fn test_report_key_set_requires_nonempty_single_namespace_and_collapses_duplicates() {
        assert_eq!(ReportKeySet::new([]), Err(ReportKeyError::Empty));
        let key = ReportKey::from_bytes(None, b"abc");
        assert_eq!(
            ReportKeySet::new([key.clone(), key.clone()])
                .unwrap()
                .keys(),
            std::slice::from_ref(&key)
        );
        let commit: CommitId = "0123456".parse().unwrap();
        assert_eq!(
            ReportKeySet::new([key, ReportKey::from_bytes(Some(&commit), b"abc")]),
            Err(ReportKeyError::MixedNamespaces)
        );
    }
}
