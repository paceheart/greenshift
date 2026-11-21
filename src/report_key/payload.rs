//! Completed key payloads. Only their validating producer can construct them.

/// An asserted report fingerprint, not proof that a test execution occurred.
/// The empty namespace explicitly means unknown commit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReportKey {
    namespace: String,
    digest: String,
}

impl ReportKey {
    /// The supported fingerprint format: SHA256 of exact captured raw bytes.
    pub const VERSION: i64 = 1;

    pub(super) fn from_validated_parts(namespace: String, digest: String) -> Self {
        Self { namespace, digest }
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    pub fn version(&self) -> i64 {
        Self::VERSION
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

/// A nonempty, canonical, deeply read-only set for a single import namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportKeySet {
    keys: Vec<ReportKey>,
}

impl ReportKeySet {
    pub(super) fn from_canonical(keys: Vec<ReportKey>) -> Self {
        Self { keys }
    }

    pub fn keys(&self) -> &[ReportKey] {
        &self.keys
    }

    pub fn namespace(&self) -> &str {
        // The validating producer guarantees nonempty keys and exposes no
        // mutable access to this completed collection.
        self.keys[0].namespace()
    }
}
