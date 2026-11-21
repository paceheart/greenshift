//! Completed transport outcomes. Grouping limitations are data, not full success.

/// A validated opaque run identity involved in a partial membership overlap.
/// It contains no test names, paths, report bytes or fingerprint lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupOverlap {
    history_id: Box<str>,
    unseen_memberships: usize,
}

impl GroupOverlap {
    // Only admitted snapshot rows supply identities here; their loader validates
    // exactly 32 lowercase hexadecimal bytes before union can inspect them.
    pub(super) fn from_validated_id(history_id: &str, unseen_memberships: usize) -> Self {
        Self {
            history_id: history_id.into(),
            unseen_memberships,
        }
    }

    pub fn history_id(&self) -> &str {
        &self.history_id
    }

    pub fn unseen_memberships(&self) -> usize {
        self.unseen_memberships
    }
}

/// Limitations of a destination-authoritative history merge.
///
/// Excluded incoming groups are not transferred. Retained groups describe the
/// opposite direction: their membership partially overlaps the source, so
/// merging them back into that source may exclude them. Neither is recovery of
/// unavailable per-report observations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeReport {
    excluded_groups: Box<[GroupOverlap]>,
    retained_groups: Box<[GroupOverlap]>,
    nonexact_replays: usize,
    metadata_replays: usize,
}

impl MergeReport {
    pub(super) fn new(
        excluded_groups: Vec<GroupOverlap>,
        retained_groups: Vec<GroupOverlap>,
        nonexact_replays: usize,
        metadata_replays: usize,
    ) -> Self {
        Self {
            excluded_groups: excluded_groups.into_boxed_slice(),
            retained_groups: retained_groups.into_boxed_slice(),
            nonexact_replays,
            metadata_replays,
        }
    }

    pub fn excluded_groups(&self) -> &[GroupOverlap] {
        &self.excluded_groups
    }

    pub fn retained_groups(&self) -> &[GroupOverlap] {
        &self.retained_groups
    }

    /// Non-exact all-seen groups were admitted as membership-only replays,
    /// without proving equivalence of their aggregated results.
    pub fn nonexact_replays(&self) -> usize {
        self.nonexact_replays
    }

    /// Replay groups whose source/partial/ref metadata differed from the
    /// retained destination metadata. No metadata is upgraded by replay.
    pub fn metadata_replays(&self) -> usize {
        self.metadata_replays
    }
}

/// A committed canonical snapshot and the limitations of its final CAS attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishOutcome {
    oid: Box<str>,
    report: MergeReport,
}

impl PublishOutcome {
    pub(super) fn new(oid: String, report: MergeReport) -> Self {
        Self {
            oid: oid.into_boxed_str(),
            report,
        }
    }

    pub fn oid(&self) -> &str {
        &self.oid
    }

    pub fn report(&self) -> &MergeReport {
        &self.report
    }
}

/// Whether canonical history was found, including any grouping limitations of
/// the committed local restore. A restored outcome does not imply full recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    Missing,
    Restored(MergeReport),
}
