use super::*;
use crate::cli::{parse, Backend as BackendCommand, Command};
use crate::tainted::Tainted;

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn backend(args: &[&str], selector: Selector) -> Lein {
    Lein::new(os(args), selector, MAX_ONLY_BYTES)
}

fn test(suite: &str, name: &str) -> InventoryTest {
    InventoryTest::new(Tainted::raw(suite), Tainted::raw(name))
}

fn lein_args(args: &[&str]) -> crate::cli::LeinRunArgs {
    let argv = ["greenshift", "run", "lein"].iter().chain(args);
    let Command::Run(run) = parse(argv).unwrap().command else {
        panic!("not run");
    };
    let BackendCommand::Lein(args) = run.backend else {
        panic!("not lein");
    };
    args
}

#[test]
fn run_lein_parses_with_its_own_source_and_passes_the_rest_on() {
    let args = lein_args(&["--min-runs", "3", "--", ":slow", "my.ns"]);
    assert_eq!(args.options.select.source, ["lein"]);
    assert_eq!(args.options.select.min_runs, 3);
    assert_eq!(args.args, os(&[":slow", "my.ns"]));
    assert_eq!(args.max_only_bytes, MAX_ONLY_BYTES);
    let args = lein_args(&["--source", "lein-memory", "--max-only-bytes", "10"]);
    assert_eq!(args.options.select.source, ["lein-memory"]);
    assert_eq!(args.max_only_bytes, 10);
    assert!(args.args.is_empty());
}

#[test]
fn selectors_and_namespaces_go_to_the_listing_only() {
    let lein = backend(&[":slow", "my.ns", ":red"], Selector::Names);
    let script = Path::new("/scratch/list.clj");
    assert_eq!(
        lein.list_command(script).unwrap(),
        os(&[
            "update-in",
            ":injections",
            "conj",
            r#"(load-file "/scratch/list.clj")"#,
            "--",
            "test",
            ":slow",
            "my.ns",
            ":red",
        ])
    );
    let script = Path::new("/scratch/rich-1.clj");
    assert_eq!(
        lein.run_command(script, &["a/b".to_owned()]).unwrap(),
        os(&[
            "update-in",
            ":injections",
            "conj",
            r#"(load-file "/scratch/rich-1.clj")"#,
            "--",
            "test",
            ":greenshift",
        ])
    );
}

#[test]
fn the_fallback_runs_the_names_with_only() {
    let lein = backend(&[":slow"], Selector::Only);
    let names = ["a.b-test/one".to_owned(), "c-test/two".to_owned()];
    let args = lein
        .run_command(Path::new("/s/poor-2.clj"), &names)
        .unwrap();
    assert_eq!(
        args[4..],
        os(&["--", "test", ":only", "a.b-test/one", "c-test/two"])
    );
}

#[test]
fn quotes_backslashes_and_newlines_in_paths_are_escaped_for_clojure() {
    let path = Path::new("/tmp/a \"q\" b\\c\nd/list.clj");
    assert_eq!(
        injection(path).unwrap(),
        OsString::from(r#"(load-file "/tmp/a \"q\" b\\c\nd/list.clj")"#)
    );
    let script = list_script(path).unwrap();
    assert!(script
        .ends_with("(greenshift.lein.list/install! \"/tmp/a \\\"q\\\" b\\\\c\\nd/list.clj\")\n"));
}

#[cfg(unix)]
#[test]
fn a_path_that_is_not_utf8_is_an_error() {
    use std::os::unix::ffi::OsStrExt;
    let path = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff/list.clj"));
    assert!(injection(path).is_err());
}

#[test]
fn one_invocation_per_class_names_exactly_its_group_in_order() {
    let lein = backend(&[":slow"], Selector::Names);
    let tests: Vec<InventoryTest> = (0..10_000)
        .map(|i| test(&format!("ns{}.core-test", i % 37), &format!("t{i}")))
        .collect();
    let group: Vec<&InventoryTest> = tests.iter().rev().collect();
    let dir = Path::new("/scratch");
    for (class, stem) in [(Class::Rich, "rich-1"), (Class::Poor, "poor-1")] {
        let invocations = lein.invocations(class, &group, dir).unwrap();
        assert_eq!(invocations.len(), 1);
        let inv = &invocations[0];
        assert_eq!(inv.program, OsString::from("lein"));
        assert_eq!(inv.junit, dir.join(format!("{stem}.xml")));
        assert_eq!(inv.args.last().unwrap(), ":greenshift");
        let [(names_path, names), (script_path, script)] = &inv.setup[..] else {
            panic!("{:?}", inv.setup);
        };
        assert_eq!(*names_path, dir.join(format!("{stem}.names")));
        assert_eq!(*script_path, dir.join(format!("{stem}.clj")));
        let expected: String = group
            .iter()
            .map(|t| format!("{}/{}\n", t.suite().as_raw(), t.name().as_raw()))
            .collect();
        assert_eq!(*names, expected);
        assert!(script.starts_with(JUNIT_CLJ));
        assert!(script.contains(&format!(
            "(System/setProperty \"greenshift.names\" \"/scratch/{stem}.names\")"
        )));
        assert!(script.contains(&format!(
            "(greenshift.lein.junit/install! {{:names \"/scratch/{stem}.names\" \
             :junit \"/scratch/{stem}.xml\"}})"
        )));
        assert_eq!(
            inv.args[3],
            OsString::from(format!("(load-file \"/scratch/{stem}.clj\")"))
        );
    }
}

#[test]
fn an_empty_group_is_one_invocation_that_names_nothing() {
    for selector in [Selector::Names, Selector::Only] {
        let invocations = backend(&[], selector)
            .invocations(Class::Rich, &[], Path::new("/s"))
            .unwrap();
        assert_eq!(invocations.len(), 1, "{selector:?}");
        assert_eq!(invocations[0].setup[0].1, "");
    }
}

#[test]
fn the_fallback_splits_a_long_group_into_batches_that_fit() {
    let tests: Vec<InventoryTest> = (0..1000)
        .map(|i| test("some.long.namespace-test", &format!("test-{i:04}")))
        .collect();
    let group: Vec<&InventoryTest> = tests.iter().collect();
    let lein = Lein::new(Vec::new(), Selector::Only, 5_000);
    let invocations = lein
        .invocations(Class::Poor, &group, Path::new("/s"))
        .unwrap();
    assert!(invocations.len() > 1);
    let mut named = Vec::new();
    for (at, inv) in invocations.iter().enumerate() {
        assert_eq!(
            inv.junit,
            Path::new("/s").join(format!("poor-{}.xml", at + 1))
        );
        let names: Vec<String> = inv.args[7..]
            .iter()
            .map(|arg| arg.to_str().unwrap().to_owned())
            .collect();
        let bytes: usize = names.iter().map(|n| n.len() + 1).sum();
        assert!(bytes <= 5_000, "{bytes}");
        // The names file holds the batch too, for ordering and skips.
        let file: Vec<String> = inv.setup[0].1.lines().map(str::to_owned).collect();
        assert_eq!(file, names);
        named.extend(names);
    }
    let all: Vec<String> = group.iter().map(|t| test_name(t)).collect();
    assert_eq!(named, all);
}

#[test]
fn batches_keep_order_and_put_an_oversized_name_alone() {
    let names: Vec<String> = ["aa", "bbbbbbbbbb", "c", "d"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        only_batches(&names, 4),
        vec![
            vec!["aa".to_owned()],
            vec!["bbbbbbbbbb".to_owned()],
            vec!["c".to_owned(), "d".to_owned()],
        ]
    );
    assert!(only_batches(&[], 4).is_empty());
}

#[test]
fn the_listing_reads_as_an_inventory() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("list.jsonl");
    std::fs::write(
        &file,
        "{\"suite\":\"a.core-test\",\"name\":\"adds\"}\n{\"suite\":\"a.core-test\",\"name\":\"q\\\"\"}\n",
    )
    .unwrap();
    let tests = read_listing(&file).unwrap();
    let names: Vec<String> = tests.iter().map(test_name).collect();
    assert_eq!(names, ["a.core-test/adds", "a.core-test/q\""]);
    std::fs::write(&file, "not json\n").unwrap();
    assert!(read_listing(&file).is_err());
    assert!(read_listing(&dir.path().join("missing.jsonl")).is_err());
}

#[test]
fn the_selector_comes_from_project_clj() {
    let dir = tempfile::tempdir().unwrap();
    let clj = dir.path().join("project.clj");
    // No project.clj here is noted: lein may find one in a parent.
    let (selector, note) = Selector::for_project(&clj);
    assert_eq!(selector, Selector::Only);
    assert!(note.unwrap().contains("project's root"));
    std::fs::write(&clj, "(defproject a \"1\" :test-selectors {:fast :fast})").unwrap();
    let (selector, note) = Selector::for_project(&clj);
    assert_eq!(selector, Selector::Only);
    assert!(note.unwrap().contains("greenshift init lein"));
    std::fs::write(&clj, "(defproject a \"1\" :test-selectors ~(sel))").unwrap();
    let (selector, note) = Selector::for_project(&clj);
    assert_eq!(selector, Selector::Only);
    // init refuses a computed map, so the note does not send the user there.
    let note = note.unwrap();
    assert!(note.contains("computed"), "{note}");
    assert!(!note.contains("init lein"), "{note}");
    let merged = match crate::lein::merge("(defproject a \"1\")", false).unwrap() {
        crate::lein::Merge::Changed(text) => text,
        other => panic!("{other:?}"),
    };
    std::fs::write(&clj, merged).unwrap();
    assert_eq!(Selector::for_project(&clj), (Selector::Names, None));
}
