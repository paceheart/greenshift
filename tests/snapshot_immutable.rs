//! Snapshot representation contracts, independent of its accounting counters.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
mod common;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use greenshift::history::snapshot;
use greenshift::store::Store;
use rusqlite::Connection;

const NAME_BYTES: usize = 4093;
static COUNT_NAME_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
static NAME_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static NAME_ALLOCATION_BYTES: AtomicUsize = AtomicUsize::new(0);

struct ObservedAllocator;

fn observe(size: usize) {
    // A broad bucket includes Arc headers/alignment as well as String payloads:
    // moving copies from len to len+header must not make this oracle pass.
    // Other row/vector allocations can occasionally fall in this interval;
    // the assertion allows those, but not one name allocation per result.
    if COUNT_NAME_ALLOCATIONS.load(Ordering::Relaxed)
        && (NAME_BYTES..=2 * NAME_BYTES).contains(&size)
    {
        NAME_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        NAME_ALLOCATION_BYTES.fetch_add(size, Ordering::Relaxed);
    }
}

// SAFETY: all operations delegate unchanged to the system allocator. Observation
// uses allocation-free atomics and never changes a pointer, size or alignment.
unsafe impl GlobalAlloc for ObservedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        observe(layout.size());
        // SAFETY: the caller supplies the allocator contract's valid layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        observe(layout.size());
        // SAFETY: the caller supplies the allocator contract's valid layout.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: this is the same allocation/layout delegated to System.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        observe(size);
        // SAFETY: pointer/layout/new size retain the caller's allocator contract.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: ObservedAllocator = ObservedAllocator;

fn repeated_name_fixture(path: &Path, results: i64) {
    drop(Store::open(path).unwrap());
    let connection = Connection::open(path).unwrap();
    connection
        .execute(
            "INSERT INTO tests(id,suite,name) VALUES(1,'fixed',?1)",
            ["n".repeat(NAME_BYTES)],
        )
        .unwrap();
    connection
        .execute(
            "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n<?1)
             INSERT INTO runs(id,source,ingested_at,history_id)
             SELECT n,'allocation-fixture','2000-01-01T00:00:00.000Z',printf('%032x',n) FROM seq",
            [results],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO results(run_id,test_id,outcome,duration_ms)
             SELECT id,1,'passed',10 FROM runs",
            [],
        )
        .unwrap();
}

#[test]
fn repeated_test_name_storage_does_not_allocate_one_payload_per_result() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("repeated.sqlite");
    repeated_name_fixture(&database, 5000);
    NAME_ALLOCATIONS.store(0, Ordering::Relaxed);
    NAME_ALLOCATION_BYTES.store(0, Ordering::Relaxed);
    COUNT_NAME_ALLOCATIONS.store(true, Ordering::Relaxed);
    let result = snapshot::validate(&database);
    COUNT_NAME_ALLOCATIONS.store(false, Ordering::Relaxed);
    result.unwrap();
    let allocations = NAME_ALLOCATIONS.load(Ordering::Relaxed);
    let bytes = NAME_ALLOCATION_BYTES.load(Ordering::Relaxed);
    eprintln!(
        "name-sized Rust heap allocations for5000 repeated-name results: {allocations}; requested_bytes={bytes}"
    );
    assert!(
        allocations < 64,
        "dictionary text must not allocate a name payload per result; observed {allocations}"
    );
    assert!(
        bytes < NAME_BYTES * 64,
        "name-copy allocation bytes scale with results: {bytes}"
    );
}

fn immutable_fixture(body: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/history/snapshot");
    format!(
        "#![allow(dead_code,unused_mut,unused_imports)]\n\
         #[path = {text:?}] mod text;\n\
         #[path = {dictionary:?}] mod dictionary;\n\
         #[path = {outcome:?}] mod junit;\n\
         #[path = {report_key:?}] mod report_key;\n\
         #[path = {frozen:?}] mod frozen;\n\
         use text::FrozenText;\n\
         use dictionary::{{DictionaryBuilder,TestKey}};\n\
         use frozen::{{FrozenBytes,FrozenFiles,FrozenReports,FrozenResults,FrozenRuns,ResultRow,Run,RunMetadata}};\n\
         fn main() {{\n\
             let mut text = FrozenText::new(\"suite\".to_owned());\n\
             let mut builder = DictionaryBuilder::default();\n\
             let id = builder.insert(TestKey::new(text.clone(),FrozenText::new(\"name\".to_owned())));\n\
             let mut dictionary = builder.freeze();\n\
             assert_eq!(dictionary.get(id).unwrap().suite().as_str(),\"suite\");\n\
             let mut bytes = FrozenBytes::copy_from(&[255,0,128]);\n\
             let alias = bytes.clone();\n\
             assert_eq!(alias.as_bytes(),&[255,0,128]);\n\
             assert!(std::ptr::eq(alias.as_bytes(),bytes.as_bytes()));\n\
             let mut files = FrozenFiles::from_vec(vec![(0,bytes.clone())]);\n\
             let mut result = ResultRow::new(id,junit::Outcome::Passed,Some(12));\n\
             let mut results = FrozenResults::from_vec(vec![result]);\n\
             let mut metadata = RunMetadata::new(FrozenText::new(\"0123456789abcdef0123456789abcdef\".to_owned()),Some(text.clone()),text.clone(),text.clone(),0,Some(text.clone()),None);\n\
             let mut reports = FrozenReports::from_vec(vec![report_key::ReportKey::from_validated_parts(String::new(),\"0\".repeat(64))]);\n\
             let mut report_keys = report_key::ReportKeySet::from_canonical(reports.as_slice().to_vec());\n\
             assert_eq!(report_keys.keys(),reports.as_slice());\n\
             assert_eq!(reports.as_slice().len(),1);\n\
             assert_eq!(reports.as_slice()[0].namespace(),\"\");\n\
             let mut run = Run::new(metadata.clone(),files.clone(),results.clone(),reports.clone());\n\
             let mut runs = FrozenRuns::from_vec(vec![run.clone()]);\n\
             assert_eq!(files.iter().next().unwrap().1.as_bytes(),&[255,0,128]);\n\
             assert_eq!(results.iter().count(),1);\n\
             assert_eq!(runs.iter().count(),1);\n\
             assert_eq!(result.test(),id);\n\
             assert_eq!(result.outcome(),junit::Outcome::Passed);\n\
             for (outcome,wire) in [(junit::Outcome::Passed,\"passed\"),(junit::Outcome::Failed,\"failed\"),(junit::Outcome::Error,\"error\"),(junit::Outcome::Skipped,\"skipped\")] {{\n\
                 assert_eq!(outcome.as_str(),wire);\n\
                 assert_eq!(outcome.to_string(),wire);\n\
             }}\n\
             assert_eq!(result.duration(),Some(12));\n\
             assert_eq!(run.metadata(),&metadata);\n\
             assert_eq!(run.metadata().history_id().as_str(),\"0123456789abcdef0123456789abcdef\");\n\
             assert_eq!(run.metadata().commit().unwrap().as_str(),\"suite\");\n\
             assert_eq!(run.metadata().source().as_str(),\"suite\");\n\
             assert_eq!(run.metadata().at().as_str(),\"suite\");\n\
             assert_eq!(run.metadata().partial(),0);\n\
             assert_eq!(run.metadata().branch().unwrap().as_str(),\"suite\");\n\
             assert!(run.metadata().tag().is_none());\n\
             assert_eq!(run.files(),&files);\n\
             assert_eq!(run.results(),&results);\n\
             assert_eq!(run.reports(),&reports);\n\
             assert_eq!(runs.iter().next().unwrap(),&run);\n\
             {body}\n\
         }}\n",
        text = root.join("text.rs").to_str().unwrap(),
        dictionary = root.join("dictionary.rs").to_str().unwrap(),
        outcome = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/junit/outcome.rs"),
        report_key = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/report_key/payload.rs"),
        frozen = root.join("frozen.rs").to_str().unwrap(),
    )
}

fn compile_fixture(
    body: &str,
    execute: bool,
) -> (std::process::Output, Option<std::process::Output>) {
    common::compile(execute, |_| immutable_fixture(body))
}

#[test]
fn immutable_text_and_dictionary_reject_actual_mutation_after_valid_setup() {
    let (positive, execution) = compile_fixture("assert_eq!(text.as_str(),\"suite\");", true);
    assert!(
        positive.status.success(),
        "positive setup must compile: {}",
        String::from_utf8_lossy(&positive.stderr)
    );
    let execution = execution.unwrap();
    assert!(
        execution.status.success(),
        "positive read/clone/binary-path contract failed"
    );
    for (body, diagnostic) in [
        ("text.as_str().make_ascii_uppercase();", "error[E0596]"),
        ("text.make_ascii_uppercase();", "error[E0599]"),
        ("dictionary.get_mut(id);", "error[E0599]"),
        (
            "dictionary.0[0]=TestKey::new(text.clone(),text.clone());",
            "error[E0616]",
        ),
        (
            "std::sync::Arc::make_mut(&mut text.0).make_ascii_uppercase();",
            "error[E0616]",
        ),
        ("bytes.as_bytes()[0]=1;", "error[E0594]"),
        ("files.iter_mut();", "error[E0599]"),
        ("results.iter_mut();", "error[E0599]"),
        ("runs.iter_mut();", "error[E0599]"),
        (
            "reports.as_slice()[0]=report_key::ReportKey::from_validated_parts(String::new(),\"1\".repeat(64));",
            "error[E0594]",
        ),
        (
            "reports.0[0]=report_key::ReportKey::from_validated_parts(String::new(),\"1\".repeat(64));",
            "error[E0616]",
        ),
        (
            "reports.as_slice()[0].digest().make_ascii_uppercase();",
            "error[E0596]",
        ),
        ("run.reports=reports.clone();", "error[E0616]"),
        (
            "report_keys.keys()[0]=report_key::ReportKey::from_validated_parts(String::new(),\"1\".repeat(64));",
            "error[E0594]",
        ),
        (
            "report_keys.namespace().make_ascii_uppercase();",
            "error[E0596]",
        ),
        ("report_keys.keys.clear();", "error[E0616]"),
        ("*run.reports()=reports.clone();", "error[E0594]"),
        (
            "std::sync::Arc::make_mut(&mut bytes.0)[0]=1;",
            "error[E0616]",
        ),
        ("files.0[0].0=7;", "error[E0616]"),
        ("results.0[0]=result;", "error[E0616]"),
        ("runs.0[0]=run.clone();", "error[E0616]"),
        ("run.metadata=metadata.clone();", "error[E0616]"),
        ("run.files=files.clone();", "error[E0616]"),
        ("run.results=results.clone();", "error[E0616]"),
        ("metadata.history_id=text.clone();", "error[E0616]"),
        ("metadata.commit=None;", "error[E0616]"),
        ("metadata.source=text.clone();", "error[E0616]"),
        ("metadata.at=text.clone();", "error[E0616]"),
        ("metadata.partial=1;", "error[E0616]"),
        ("metadata.branch=None;", "error[E0616]"),
        ("metadata.tag=Some(text.clone());", "error[E0616]"),
        ("result.test=id;", "error[E0616]"),
        ("result.outcome=junit::Outcome::Failed;", "error[E0616]"),
        ("result.duration=None;", "error[E0616]"),
        ("*run.metadata()=metadata.clone();", "error[E0594]"),
        ("*run.files()=files.clone();", "error[E0594]"),
        ("*run.results()=results.clone();", "error[E0594]"),
        ("*run.metadata().source()=text.clone();", "error[E0594]"),
        (
            "*run.results().iter().next().unwrap()=result;",
            "error[E0594]",
        ),
    ] {
        let (negative, _) = compile_fixture(body, false);
        let stderr = String::from_utf8_lossy(&negative.stderr);
        assert!(
            !negative.status.success(),
            "mutation unexpectedly compiled: {body}"
        );
        assert!(
            stderr.contains(diagnostic),
            "wrong failure for{body}: {stderr}"
        );
        assert!(!stderr.contains("unresolved import"));
        assert!(!stderr.contains("couldn't read"));
    }
}
