//! Completed inventory, plan and listing data is sealed at producer-parent privacy.

use std::path::Path;
mod common;

fn fixture(body: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    // The fixture is the actual child modules' producer parent, not an external
    // caller: widening a field to pub(super) must reopen these mutation probes.
    format!(
        r#"
#![allow(dead_code, unused_mut)]
#[path={tainted:?}] mod tainted;
#[path={plan:?}] mod plan_payload;
#[path={listing:?}] mod listing_payload;
use tainted::Tainted;
use plan_payload::{{InventoryTest, Inventory, Plan}};
use listing_payload::{{BinaryIds, Listing, ListingBuilder}};
fn main() {{
    let mut test=InventoryTest::new(Tainted::raw("suite\t"),Tainted::raw("name\n"));
    assert_eq!(test.suite().as_raw(),"suite\t");
    assert_eq!(test.name().to_string(),"name\\n");
    let mut builder=Vec::new();
    builder.push(test.clone());
    builder.push(InventoryTest::new(Tainted::raw("other"),Tainted::raw("second")));
    let mut inventory=Inventory::new(builder);
    assert_eq!(inventory.len(),2);
    assert!(!inventory.is_empty());
    assert_eq!(inventory.as_slice()[0],test);
    assert_eq!(inventory.iter().next(),Some(&test));
    assert_eq!((&inventory).into_iter().count(),2);
    let mut consumed=inventory.clone().into_tests();
    assert_eq!(consumed.len(),2);
    assert_eq!(consumed.next(),Some(test.clone()));
    assert_eq!(consumed.next_back().unwrap().name().as_raw(),"second");
    assert!(consumed.next().is_none());
    drop(consumed);
    assert!(Inventory::default().is_empty());
    let mut plan=Plan::new(vec![&inventory[1]],vec![&inventory[0]]);
    assert_eq!(plan.rich(),[&inventory[1]]);
    assert_eq!(plan.poor(),[&inventory[0]]);
    let borrowed={{let short=Plan::new(vec![&inventory[0]],vec![]);short.rich()[0]}};
    assert_eq!(borrowed,&test);
    let mut listing_builder=ListingBuilder::default();
    listing_builder.binaries.insert("suite".to_owned());
    listing_builder.binaries.insert("other".to_owned());
    listing_builder.tests.insert("suite".to_owned(),["name".to_owned()].into());
    let mut listing=listing_builder.finish();
    assert!(listing.has("suite","name"));
    assert!(!listing.has("suite","gone"));
    assert!(listing.has("other","unknown"));
    assert!(listing.has("","anything"));
    assert_eq!(listing.tests()["suite"].len(),1);
    let mut binaries=listing.clone().into_binary_ids();
    assert_eq!(binaries.len(),2);
    assert!(!binaries.is_empty());
    assert!(binaries.contains("suite"));
    assert_eq!(&binaries,listing.binaries());
    let mut borrowed=Vec::new();
    for id in &binaries {{ borrowed.push(id.as_str()); }}
    assert_eq!(borrowed,["other","suite"]);
    assert_eq!(binaries.iter().map(String::as_str).collect::<Vec<_>>(),["other","suite"]);
    let mut ids=binaries.clone().into_ids();
    assert_eq!(ids.len(),2);
    assert_eq!(ids.next().as_deref(),Some("other"));
    assert_eq!(ids.next_back().as_deref(),Some("suite"));
    drop(ids);
    assert!(Listing::default().binaries().is_empty());
    assert!(BinaryIds::default().is_empty());
    {body}
}}
"#,
        tainted = root.join("tainted.rs"),
        plan = root.join("plan/payload.rs"),
        listing = root.join("nextest/payload.rs")
    )
}

fn compile(body: &str, execute: bool) -> (std::process::Output, Option<std::process::Output>) {
    common::compile(execute, |_| fixture(body))
}

#[test]
fn completed_plan_listing_payloads_reject_mutation_after_valid_builder_read_consume_setup() {
    let (positive, execution) = compile("", true);
    assert!(
        positive.status.success(),
        "valid setup: {}",
        String::from_utf8_lossy(&positive.stderr)
    );
    let execution = execution.unwrap();
    assert!(
        execution.status.success(),
        "read/consume contract: {}",
        String::from_utf8_lossy(&execution.stderr)
    );
    for (body, diagnostic) in [
        ("listing.tests.clear();", "error[E0616]"),
        ("listing.binaries=BinaryIds::default();", "error[E0616]"),
        ("binaries.ids.clear();", "error[E0616]"),
        ("test.suite=Tainted::raw(\"changed\");", "error[E0616]"),
        ("test.name=Tainted::raw(\"changed\");", "error[E0616]"),
        ("inventory.tests.clear();", "error[E0616]"),
        ("plan.rich.clear();", "error[E0616]"),
        ("plan.poor.clear();", "error[E0616]"),
        ("listing.tests().clear();", "error[E0596]"),
        ("listing.tests()[\"suite\"].clear();", "error[E0596]"),
        (
            "listing.binaries().iter().next().unwrap().clear();",
            "error[E0596]",
        ),
        ("for id in &binaries { id.clear(); }", "error[E0596]"),
        (
            "test.name().as_raw().make_ascii_uppercase();",
            "error[E0596]",
        ),
        (
            "test.suite().as_raw().make_ascii_uppercase();",
            "error[E0596]",
        ),
        ("inventory.as_slice()[0]=test.clone();", "error[E0594]"),
        ("inventory[0]=test.clone();", "error[E0594]"),
        ("plan.rich()[0]=&inventory[0];", "error[E0594]"),
        ("plan.poor()[0]=&inventory[1];", "error[E0594]"),
        ("inventory.into_tests().as_mut_slice();", "error[E0599]"),
        ("binaries.into_ids().as_mut_slice();", "error[E0599]"),
        ("inventory.iter_mut();", "error[E0596]"),
        ("listing.tests_mut();", "error[E0599]"),
        ("plan.rich_mut();", "error[E0599]"),
    ] {
        let (negative, _) = compile(body, false);
        let error = String::from_utf8_lossy(&negative.stderr);
        assert!(
            !negative.status.success(),
            "mutation unexpectedly compiled: {body}"
        );
        assert!(
            error.contains(diagnostic),
            "wrong failure for {body}: {error}"
        );
        assert!(!error.contains("unresolved import"));
        assert!(!error.contains("couldn't read"));
    }
}

#[test]
fn production_inventory_listing_and_plan_preserve_parsing_order_and_consumption() {
    use greenshift::{nextest, plan};
    let inventory = plan::parse_inventory("{\"suite\":\"b\",\"name\":\"two\"}\n{\"suite\":\"a\",\"name\":\"one\"}\n{\"suite\":\"b\",\"name\":\"two\"}").unwrap();
    let planned = plan::plan(&[], &inventory);
    assert_eq!(planned.rich(), [&inventory[0], &inventory[1]]);
    assert!(planned.poor().is_empty());
    let mut groups = greenshift::run::planned_groups(planned.rich(), planned.poor());
    assert_eq!(groups.len(), 1);
    assert_eq!(
        groups.next_back(),
        Some((greenshift::select::Class::Rich, planned.rich()))
    );
    assert_eq!(groups.len(), 0);
    let mut groups = greenshift::run::planned_groups(&planned.rich()[..1], &planned.rich()[1..]);
    assert_eq!(groups.len(), 2);
    assert_eq!(
        groups.next_back(),
        Some((greenshift::select::Class::Poor, &planned.rich()[1..]))
    );
    assert_eq!(
        groups.next(),
        Some((greenshift::select::Class::Rich, &planned.rich()[..1]))
    );
    assert_eq!(groups.len(), 0);
    assert_eq!(
        planned.to_json(plan::HistoryState::Ok),
        r#"{"history":"ok","rich":[{"suite":"b","name":"two"},{"suite":"a","name":"one"}],"poor":[]}"#
    );
    let json = r#"{"rust-suites":{"b":{"binary-id":"b","status":"listed","testcases":{"two":{"filter-match":{"status":"matches"}}}},"a":{"binary-id":"a","status":"listed","testcases":{"one":{"filter-match":{"status":"matches"}}}}}}"#;
    let listing = nextest::listing(json).unwrap();
    assert_eq!(
        nextest::binary_ids(json).unwrap(),
        listing.clone().into_binary_ids()
    );
    assert_eq!(
        listing
            .binaries()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    let selected = nextest::selected_tests(json).unwrap();
    assert_eq!(
        selected
            .iter()
            .map(|t| (t.suite().as_raw(), t.name().as_raw()))
            .collect::<Vec<_>>(),
        [("a", "one"), ("b", "two")]
    );
    assert_eq!(
        selected.clone().into_tests().collect::<Vec<_>>(),
        selected.as_slice()
    );
    assert_eq!(
        greenshift::run::planned_groups(planned.rich(), planned.poor())
            .map(|(class, group)| (class, group.len()))
            .collect::<Vec<_>>(),
        [(greenshift::select::Class::Rich, 2)]
    );
}

#[cfg(unix)]
#[test]
fn custom_backend_keeps_list_invocation_and_empty_execution_contract() {
    use greenshift::cli::{self, Backend as BackendCommand, Command as CliCommand};
    use greenshift::plan::{Inventory, InventoryTest};
    use greenshift::run::{self, Backend, Invocation, Listed};
    use greenshift::select::Class;
    use greenshift::store::Store;
    use greenshift::tainted::Tainted;
    use std::cell::RefCell;

    struct Custom {
        tests: Inventory,
        seen: RefCell<Vec<(Class, Vec<String>)>>,
    }
    impl Backend for Custom {
        fn name(&self) -> &'static str {
            "custom"
        }
        fn list(&self, dir: &Path) -> anyhow::Result<Listed> {
            assert!(dir.is_absolute() && dir.is_dir());
            Ok(Listed::Tests(self.tests.clone()))
        }
        fn invocations(
            &self,
            class: Class,
            tests: &[&InventoryTest],
            dir: &Path,
        ) -> anyhow::Result<Vec<Invocation>> {
            self.seen.borrow_mut().push((
                class,
                tests.iter().map(|t| t.name().as_raw().to_owned()).collect(),
            ));
            let junit = dir.join("custom.xml");
            Ok(vec![Invocation {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 0".into()],
                setup: vec![(
                    junit.clone(),
                    r#"<testsuite name="suite"><testcase name="one" time="0.003"/></testsuite>"#
                        .to_owned(),
                )],
                junit,
            }])
        }
    }
    for empty in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let db = directory.path().join("history.sqlite");
        let CliCommand::Run(args) = cli::parse([
            "greenshift",
            "run",
            "nextest",
            "--db",
            db.to_str().unwrap(),
            "--source",
            "custom",
            "--branch",
            "fixture",
        ])
        .unwrap()
        .command
        else {
            panic!("run command");
        };
        let BackendCommand::Nextest(args) = args.backend else {
            panic!("not nextest");
        };
        let custom = Custom {
            tests: Inventory::new(if empty {
                vec![]
            } else {
                vec![InventoryTest::new(
                    Tainted::raw("suite"),
                    Tainted::raw("one"),
                )]
            }),
            seen: RefCell::new(vec![]),
        };
        assert_eq!(
            run::run(&custom, &args.options).unwrap(),
            std::process::ExitCode::SUCCESS
        );
        assert_eq!(
            *custom.seen.borrow(),
            [(
                Class::Rich,
                if empty {
                    vec![]
                } else {
                    vec!["one".to_owned()]
                }
            )]
        );
        let history = Store::open(&db)
            .unwrap()
            .history(&["custom".to_owned()])
            .unwrap();
        assert_eq!(history.runs().len(), usize::from(!empty));
        if !empty {
            assert_eq!(history.runs()[0].source(), "custom");
            assert!(!history.runs()[0].partial());
            assert_eq!(history.runs()[0].results().len(), 1);
        }
    }
}
