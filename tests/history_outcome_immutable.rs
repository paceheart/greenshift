//! Actual completed transport reports expose shared reads, not mutable storage.

mod common;

fn fixture(body: &str) -> String {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/history/outcome.rs");
    format!(
        r#"#![allow(dead_code,unused_mut)]
        #[path = {source:?}] mod outcome;
        use outcome::{{GroupOverlap,MergeReport,PublishOutcome,RestoreOutcome}};
        fn main() {{
            let mut group = GroupOverlap::from_validated_id("0123456789abcdef0123456789abcdef",1);
            let mut report = MergeReport::new(vec![group.clone()],vec![group.clone()],2,1);
            let mut published = PublishOutcome::new("0123456789abcdef0123456789abcdef01234567".to_owned(),report.clone());
            assert_eq!(report.excluded_groups()[0].history_id(),group.history_id());
            assert_eq!(report.retained_groups()[0].unseen_memberships(),1);
            assert_eq!(report.nonexact_replays(),2);
            assert_eq!(report.metadata_replays(),1);
            assert_eq!(published.report(),&report);
            assert_eq!(published.oid().len(),40);
            let restored = RestoreOutcome::Restored(report.clone());
            assert!(matches!(restored,RestoreOutcome::Restored(_)));
            {body}
        }}"#,
    )
}

#[test]
fn completed_transport_outcomes_reject_mutation_after_valid_read_setup() {
    let (positive, run) = common::compile(true, |_| fixture(""));
    assert!(
        positive.status.success(),
        "positive production setup: {}",
        String::from_utf8_lossy(&positive.stderr)
    );
    assert!(run.unwrap().status.success());
    for (body, diagnostic) in [
        ("group.history_id().make_ascii_uppercase();", "error[E0596]"),
        ("group.history_id.make_ascii_uppercase();", "error[E0616]"),
        ("group.unseen_memberships=0;", "error[E0616]"),
        ("report.excluded_groups()[0]=group.clone();", "error[E0594]"),
        ("report.excluded_groups[0]=group.clone();", "error[E0616]"),
        ("report.retained_groups()[0]=group.clone();", "error[E0594]"),
        ("report.excluded_groups_mut();", "error[E0599]"),
        ("report.nonexact_replays=0;", "error[E0616]"),
        ("report.metadata_replays=0;", "error[E0616]"),
        ("published.oid().make_ascii_uppercase();", "error[E0596]"),
        ("published.oid.make_ascii_uppercase();", "error[E0616]"),
        ("published.report=MergeReport::default();", "error[E0616]"),
    ] {
        let (negative, _) = common::compile(false, |_| fixture(body));
        let errors = String::from_utf8_lossy(&negative.stderr);
        assert!(!negative.status.success(), "mutation compiled: {body}");
        assert!(
            errors.contains(diagnostic),
            "wrong reason for {body}: {errors}"
        );
    }
}
