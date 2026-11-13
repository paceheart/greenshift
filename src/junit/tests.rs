use super::*;

const VAELII: &str = include_str!("../../fixtures/vaelii-run-vars.xml");
const NEXTEST: &str = include_str!("../../fixtures/nextest.xml");
const MALFORMED: &str = include_str!("../../fixtures/malformed.xml");

fn case<'a>(report: &'a Report, suite: &str, name: &str) -> &'a TestCase {
    report
        .cases
        .iter()
        .find(|c| c.suite == suite && c.name == name)
        .unwrap_or_else(|| panic!("no case {suite} / {name} in {report:#?}"))
}

fn one(xml: &str) -> TestCase {
    let report = parse_str(xml).expect("parses");
    assert_eq!(report.cases.len(), 1, "{report:#?}");
    report.cases.into_iter().next().unwrap()
}

// --- vaelii-run-vars -------------------------------------------------------

#[test]
fn vaelii_reads_every_testcase_in_order() {
    let report = parse_str(VAELII).unwrap();
    let names: Vec<_> = report.cases.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "unknown-var",
            "parses-a-sprocket",
            "rejects-a-bent-cog",
            "spins-under-load",
            "fixtures",
            "stores-momentum",
            "counts-teeth",
        ]
    );
}

#[test]
fn vaelii_suite_is_the_namespace_from_classname() {
    let report = parse_str(VAELII).unwrap();
    let c = case(&report, "widgets.sprocket-test", "parses-a-sprocket");
    assert_eq!(c.outcome, Outcome::Passed);
    assert_eq!(c.duration_ms, Some(12));
}

#[test]
fn vaelii_assertion_failures_are_failed() {
    let report = parse_str(VAELII).unwrap();
    let c = case(&report, "widgets.sprocket-test", "rejects-a-bent-cog");
    assert_eq!(c.outcome, Outcome::Failed);
    assert_eq!(c.duration_ms, Some(104));
}

#[test]
fn vaelii_exception_is_an_error() {
    let report = parse_str(VAELII).unwrap();
    let c = case(&report, "widgets.sprocket-test", "spins-under-load");
    assert_eq!(c.outcome, Outcome::Error);
    assert_eq!(c.duration_ms, Some(1250));
}

#[test]
fn vaelii_fixture_error_and_its_unrun_vars() {
    let report = parse_str(VAELII).unwrap();
    let fixtures = case(&report, "widgets.flywheel-test", "fixtures");
    assert_eq!(fixtures.outcome, Outcome::Error);
    let unrun = case(&report, "widgets.flywheel-test", "stores-momentum");
    assert_eq!(unrun.outcome, Outcome::Skipped);
}

#[test]
fn vaelii_unresolved_var_is_skipped_with_zero_time() {
    let report = parse_str(VAELII).unwrap();
    let c = case(&report, "ghost-gate", "unknown-var");
    assert_eq!(c.outcome, Outcome::Skipped);
    assert_eq!(c.duration_ms, Some(0));
}

// --- cargo-nextest ---------------------------------------------------------

#[test]
fn nextest_reads_every_testcase_across_suites() {
    let report = parse_str(NEXTEST).unwrap();
    assert_eq!(report.cases.len(), 6);
    let suites: Vec<_> = report.cases.iter().map(|c| c.suite.as_str()).collect();
    assert_eq!(
        suites,
        [
            "sprocket-works",
            "sprocket-works",
            "sprocket-works",
            "sprocket-works",
            "sprocket-works::assembly",
            "sprocket-works::assembly",
        ]
    );
}

#[test]
fn nextest_pass_ignores_system_out_and_err() {
    let report = parse_str(NEXTEST).unwrap();
    let c = case(&report, "sprocket-works", "widgets::parses_a_sprocket");
    assert_eq!(c.outcome, Outcome::Passed);
    assert_eq!(c.duration_ms, Some(4));
}

#[test]
fn nextest_failure_with_output_is_failed() {
    let report = parse_str(NEXTEST).unwrap();
    let c = case(&report, "sprocket-works", "widgets::rejects_a_bent_cog");
    assert_eq!(c.outcome, Outcome::Failed);
    assert_eq!(c.duration_ms, Some(12));
}

#[test]
fn nextest_flaky_test_that_passed_on_retry_is_passed() {
    let report = parse_str(NEXTEST).unwrap();
    let c = case(&report, "sprocket-works", "widgets::spins_eventually");
    assert_eq!(c.outcome, Outcome::Passed);
    assert_eq!(c.duration_ms, Some(250));
}

#[test]
fn nextest_skipped() {
    let report = parse_str(NEXTEST).unwrap();
    let c = case(&report, "sprocket-works", "widgets::needs_a_lathe");
    assert_eq!(c.outcome, Outcome::Skipped);
}

#[test]
fn nextest_abort_is_an_error_and_fractional_seconds_round() {
    let report = parse_str(NEXTEST).unwrap();
    let c = case(&report, "sprocket-works::assembly", "assembles_a_gearbox");
    assert_eq!(c.outcome, Outcome::Error);
    assert_eq!(c.duration_ms, Some(1457));
    let quick = case(&report, "sprocket-works::assembly", "lists_parts");
    assert_eq!(quick.duration_ms, Some(2));
}

// --- edge cases ------------------------------------------------------------

#[test]
fn flat_testsuite_root() {
    let c = one(
        r#"<testsuite name="widgets"><testcase classname="widgets.Cog" name="turns" time="0.5"/></testsuite>"#,
    );
    assert_eq!(c.suite, "widgets.Cog");
    assert_eq!(c.duration_ms, Some(500));
}

#[test]
fn nested_testsuites_use_the_innermost_suite_name_without_classname() {
    let report = parse_str(
        r#"<testsuites>
             <testsuite name="outer">
               <testcase name="at_outer"/>
               <testsuite name="inner">
                 <testcase name="at_inner"/>
               </testsuite>
               <testcase name="back_at_outer"/>
             </testsuite>
           </testsuites>"#,
    )
    .unwrap();
    let got: Vec<_> = report
        .cases
        .iter()
        .map(|c| (c.suite.as_str(), c.name.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("outer", "at_outer"),
            ("inner", "at_inner"),
            ("outer", "back_at_outer"),
        ]
    );
}

#[test]
fn classname_wins_over_suite_name() {
    let c = one(r#"<testsuite name="bin"><testcase classname="widgets" name="x"/></testsuite>"#);
    assert_eq!(c.suite, "widgets");
}

#[test]
fn empty_classname_falls_back_to_suite_name() {
    let c = one(r#"<testsuite name="bin"><testcase classname="" name="x"/></testsuite>"#);
    assert_eq!(c.suite, "bin");
}

#[test]
fn missing_time_is_none() {
    let c = one(r#"<testsuite name="s"><testcase name="untimed"/></testsuite>"#);
    assert_eq!(c.duration_ms, None);
    let c = one(r#"<testsuite name="s"><testcase name="blank" time=""/></testsuite>"#);
    assert_eq!(c.duration_ms, None);
}

#[test]
fn time_in_fractional_seconds_rounds_to_nearest_ms() {
    for (time, ms) in [
        ("0", 0),
        ("2", 2000),
        ("0.0004", 0),
        ("0.0005", 1),
        ("0.123456", 123),
        ("12.3456789", 12346),
        ("1e-2", 10),
    ] {
        let c = one(&format!(
            r#"<testsuite name="s"><testcase name="t" time="{time}"/></testsuite>"#
        ));
        assert_eq!(c.duration_ms, Some(ms), "time={time}");
    }
}

#[test]
fn bad_time_is_an_error_naming_the_test() {
    for time in ["fast", "-1", "NaN", "inf"] {
        let err = parse_str(&format!(
            r#"<testsuite name="s"><testcase name="sprocket" time="{time}"/></testsuite>"#
        ))
        .unwrap_err();
        assert!(matches!(err, JunitError::BadTime { .. }), "{time}: {err}");
        assert!(err.to_string().contains("sprocket"), "{err}");
    }
}

#[test]
fn failure_and_error_together_is_failed() {
    let c = one(r#"<testsuite name="s"><testcase name="t">
             <error message="boom"/>
             <failure message="wrong"/>
           </testcase></testsuite>"#);
    assert_eq!(c.outcome, Outcome::Failed);
}

#[test]
fn error_wins_over_skipped() {
    let c =
        one(r#"<testsuite name="s"><testcase name="t"><skipped/><error/></testcase></testsuite>"#);
    assert_eq!(c.outcome, Outcome::Error);
}

#[test]
fn self_closing_and_open_skipped() {
    let c = one(r#"<testsuite name="s"><testcase name="t"><skipped/></testcase></testsuite>"#);
    assert_eq!(c.outcome, Outcome::Skipped);
    let c = one(
        r#"<testsuite name="s"><testcase name="t"><skipped>needs a lathe</skipped></testcase></testsuite>"#,
    );
    assert_eq!(c.outcome, Outcome::Skipped);
}

#[test]
fn failure_text_with_cdata_entities_and_markup_is_ignored() {
    let c = one(
        r#"<testsuite name="s"><testcase name="t"><failure message="a &amp; b"><![CDATA[a < b]]> &amp; &#x263A; <b>bold</b></failure></testcase></testsuite>"#,
    );
    assert_eq!(c.outcome, Outcome::Failed);
}

#[test]
fn failure_nested_deeper_than_a_direct_child_is_ignored() {
    let c = one(
        r#"<testsuite name="s"><testcase name="t"><flakyFailure><failure/></flakyFailure><rerunFailure/></testcase></testsuite>"#,
    );
    assert_eq!(c.outcome, Outcome::Passed);
}

#[test]
fn properties_and_suite_level_output_are_ignored() {
    let report = parse_str(
        r#"<testsuites><testsuite name="s">
             <properties><property name="seed" value="42"/></properties>
             <testcase name="t"/>
             <system-out>noise</system-out>
           </testsuite></testsuites>"#,
    )
    .unwrap();
    assert_eq!(report.cases.len(), 1);
}

#[test]
fn empty_testsuites_has_no_cases() {
    assert!(parse_str("<testsuites/>").unwrap().cases.is_empty());
    assert!(
        parse_str(r#"<?xml version="1.0"?><testsuites></testsuites>"#)
            .unwrap()
            .cases
            .is_empty()
    );
}

// --- malformed input -------------------------------------------------------

#[test]
fn malformed_fixture_is_an_xml_error() {
    let err = parse_str(MALFORMED).unwrap_err();
    assert!(matches!(err, JunitError::Xml { .. }), "{err:?}");
    assert!(err.to_string().starts_with("malformed XML"), "{err}");
}

#[test]
fn truncated_document_is_an_error() {
    let err = parse_str(r#"<testsuites><testsuite name="s"><testcase name="t"/>"#).unwrap_err();
    assert!(
        matches!(err, JunitError::Truncated | JunitError::Xml { .. }),
        "{err:?}"
    );
}

#[test]
fn empty_document_is_an_error() {
    assert!(matches!(parse_str("").unwrap_err(), JunitError::Empty));
    assert!(matches!(
        parse_str(r#"<?xml version="1.0"?>"#).unwrap_err(),
        JunitError::Empty
    ));
}

#[test]
fn non_junit_root_is_an_error() {
    let err = parse_str("<html><body/></html>").unwrap_err();
    assert!(
        matches!(err, JunitError::WrongRoot(ref r) if r == "html"),
        "{err:?}"
    );
}

#[test]
fn testcase_without_name_is_an_error() {
    let err =
        parse_str(r#"<testsuite name="s"><testcase classname="c"/></testsuite>"#).unwrap_err();
    assert!(matches!(err, JunitError::MissingName { .. }), "{err:?}");
}

#[test]
fn unknown_entity_is_an_error() {
    let err = parse_str(r#"<testsuite name="s"><testcase name="t"><failure>&bogus;</failure></testcase></testsuite>"#)
        .unwrap_err();
    assert!(matches!(err, JunitError::Xml { .. }), "{err:?}");
}

#[test]
fn not_utf8_file_is_an_error() {
    let dir = std::env::temp_dir().join(format!("greenshift-junit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("latin1.xml");
    std::fs::write(&path, b"<testsuite name=\"caf\xe9\"/>").unwrap();
    let err = parse_file(&path).unwrap_err();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(matches!(err, JunitError::NotUtf8), "{err:?}");
}

#[test]
fn missing_file_is_an_io_error_naming_the_path() {
    let err = parse_file(Path::new("/nonexistent/greenshift/junit.xml")).unwrap_err();
    assert!(matches!(err, JunitError::Io { .. }), "{err:?}");
    assert!(err
        .to_string()
        .contains("/nonexistent/greenshift/junit.xml"));
}

#[test]
fn outcome_text() {
    let all = [
        Outcome::Passed,
        Outcome::Failed,
        Outcome::Error,
        Outcome::Skipped,
    ];
    let text: Vec<_> = all.iter().map(|o| o.to_string()).collect();
    assert_eq!(text, ["passed", "failed", "error", "skipped"]);
}
