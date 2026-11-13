# greenshift

greenshift picks which tests to run from what your tests have done before.

It is early. This release records test history from JUnit XML into a
per-repo SQLite store (`greenshift ingest`) and chooses tests from that
history (`greenshift select`).

## Ingest

```
greenshift ingest <JUNIT_XML>... [--db PATH] [--commit SHA] [--source NAME]
```

Every file given in one invocation is recorded as one run. For example, with
cargo-nextest's JUnit output turned on in `.config/nextest.toml`:

```toml
[profile.default.junit]
path = "junit.xml"
```

```
cargo nextest run
greenshift ingest target/nextest/default/junit.xml \
    --commit "$(git rev-parse HEAD)" --source nextest
```

- `--db` defaults to `.greenshift/history.sqlite`; the directory is created.
  greenshift only writes to its own stores: a SQLite file that already holds
  another program's tables is refused with exit 1 and left untouched.
- `--commit` is optional. `--source` names the producer or CI job
  (default `junit`).
- A test is identified by suite and name. The suite is the testcase's
  `classname`, or the enclosing `<testsuite>` name when there is none; it is
  empty when neither is given.
- Outcomes are `passed`, `failed`, `error` and `skipped`; durations are
  stored in whole milliseconds. Failure text and test output are not stored.
- greenshift exits 1 and writes nothing if any file is unreadable or not
  UTF-8, is XML that quick-xml rejects, has no root element or ends before
  it is closed, has a root other than `<testsuites>` or `<testsuite>`, has
  content outside the root element, or has a `<testcase>` without a name or
  with a `time` that is not seconds. Inside a
  `<testcase>`, only which `<failure>`, `<error>` and `<skipped>` children it
  has is read; anything else there, nested testcases included, is ignored.
- An entity or character reference that cannot be resolved, in element text
  or in an attribute greenshift reads, is also an error. Only XML's five
  predefined entities (`&amp;` and so on) are known; entities declared in a
  DTD are not supported. Other attributes are not read. greenshift does not
  otherwise validate XML well-formedness: the input is expected to come from
  a known producer.

Known producers: cargo-nextest and vaelii's test runner. Anything that writes
the common `<testsuites>/<testsuite>/<testcase>` shape should work.

## Select

```
greenshift select [--db PATH] [--source NAME]... [--min-runs N]
                  [--confidence N] [--estimator wilson|empirical|smoothed]
```

`select` reads the store and writes nothing. It sorts the tests of your
latest full run into *rich* tests, which together have caught every failure
on record, and *poor* tests, which can run after them:

- **Full runs.** Runs are taken oldest first. The first is full; after that
  a run is full when it has at least 90% as many results as the largest of
  the three full runs before it. A partial run (say, of only the selected
  tests) is not full. With `--source`, given once or more, only runs from
  those sources are read; by default all are.
- **Universe.** The tests of the newest full run.
- **Rows.** Every full run in which a universe test failed (`failed` and
  `error` count alike; `skipped` counts as neither a pass nor a failure).
  Full runs with no universe failure are *green*.
- **Rich.** Tests are picked one at a time, highest failure chance per
  second first: f / (f + p) / median, where f is the test's failures in the
  rows not yet covered and p its passes in every full run. Ties go to the
  shorter median, then suite, then name, in byte order. Every row the pick
  fails in is then covered. Picking stops when no row is left, so the rich
  tests together fail in every failing full run; a test that fails in no
  row left is never picked.
- **Poor.** Every other universe test with at least `--min-runs` results
  (default 120) of any outcome, in any run read. The rest are *new* and not
  listed.
- **Median.** A test's duration is the median of its last 5 results that
  have one, in any run read; a test with none gets the median of the other
  tests' medians (1 ms if no test has one). 0 ms counts as 1 ms.

Output is one line per test, rich in pick order, then poor by median, suite
and name, with five tab-separated columns:

```
class	coverage	confidence	suite	name
```

- `class`: `rich` or `poor`.
- `coverage`: the percentage of rows covered once this test and every line
  before it pass, to one decimal. It reads `100.0` only when no row is left,
  and on every poor line.
- `confidence`: an estimate, as a percentage to one decimal, of how likely
  the whole suite is to be green once this test and every line before it
  pass, from the number of green full runs G and the rows left R: of the
  G + R full runs in which those tests passed, G were green. By default
  (`--estimator wilson`) the estimate is the lower end of the Wilson score
  interval for that share, with z = 1.96: a cautious figure that stays low
  while there are few runs to go on, and is 0 when none of them was green.
  `--estimator empirical` is the plain share G / (G + R), taken as 100%
  when both are 0, and `--estimator smoothed` is (G + 1) / (G + R + 2).
- `suite` and `name`: as stored, with control characters, tabs and
  backslashes escaped as in Rust's `{:?}`, so each test is one line.

With `--confidence N` (a percentage from 0 to 100), rich ends at the first
test whose confidence is at least N, and the rest of the picked tests are
treated like any other non-rich test: poor with enough results, otherwise
new. If no test reaches N, rich is all the picked tests and stderr says the
highest confidence reached.

Exit status:

- 0: the selection was printed (possibly nothing, when the universe is
  empty).
- 1: an error, such as a `--db` file that is not a greenshift store.
- 2: no full run from the chosen sources, or no store at all. stderr says
  `no history: run the full suite` and nothing is printed to stdout.

## Building

```
cargo build --release
```

SQLite is bundled, so release builds for `x86_64-unknown-linux-musl` are
fully static.

## License

MIT; see [LICENSE](LICENSE).
