# greenshift

greenshift picks which tests to run from what your tests have done before.

It is early. This release records test history from JUnit XML into a
per-repo SQLite store (`greenshift ingest`), chooses tests from that
history (`greenshift select`) and writes that choice as a cargo-nextest
filterset (`greenshift nextest-filter`) or, for any test runner, as JSON
rich and poor groups of the current tests (`greenshift plan`).
`greenshift run nextest` does the whole cycle with cargo-nextest: list,
plan, run and record, and `greenshift init nextest` sets a repository up
to do it in CI. `greenshift run lein` and `greenshift init lein` do the
same for Leiningen projects.

## Ingest

```
greenshift ingest <JUNIT_XML>... [--db PATH] [--commit SHA] [--source NAME]
                  [--branch NAME | --tag NAME] [--partial]
```

A bundle of previously unseen unique reports is recorded as one aggregate run.
Repeated files collapse; an all-seen bundle is a no-op, while mixing seen and
unseen reports conflicts without recording anything. For example, with
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
- `--commit` is optional. It is 7 to 64 hex digits (a full SHA-1 or
  SHA-256 object name, or an abbreviation of one at least 7 long), stored in
  lower case; anything else, such as `HEAD` or a tag, is a usage error
  (exit 2). Runs recorded before this check keep whatever they were given.
  `--source` names the producer or CI job (default `junit`).
- `--branch` names the branch the tests ran on, and the output says it
  (`... on branch NAME`). It is not part of `--source`: the source says
  what produced the results, the branch what they ran on. Without the
  flag it comes from the CI environment: `GITHUB_HEAD_REF` when it is
  nonempty (a pull request's branch, on GitHub Actions), else
  `GITHUB_REF_NAME` (the branch pushed). Forgejo and Gitea Actions set
  the same variables. An empty variable counts as unset; with neither
  flag nor variable, the run has no branch. Runs recorded before
  greenshift stored branches have none.
- `--tag` names the tag the tests ran on instead (`... on tag NAME`).
  Without either flag, a tag push is one: when `GITHUB_REF_TYPE` is
  `tag`, `GITHUB_REF_NAME` is the run's tag and the run has no branch.
  A run has a branch or a tag, never both, so giving both flags is a
  usage error (exit 2), and either flag replaces what the CI
  environment says. Only the tag pushed is recorded, not other tags on
  the commit.
- `--partial` records the run as holding only part of the suite: one pass
  of several when another wrote no JUnit, say. `select` never counts a
  partial run as full, whatever its size; its results still count towards
  `--min-runs`.
- Imports are idempotent per report: the key is SHA-256 of its exact bytes
  together with the normalized textual `--commit`. Paths and file order do
  not enter the key; repeated identical files in one invocation count once.
  Byte-different XML is distinct even when its parsed tests are equal.
  Without `--commit`, all imports share one unknown-commit namespace.
  Abbreviated and full commit strings remain distinct; no Git lookup occurs.
- When every report is new, all files form one run. When every report was
  already imported, greenshift reports `already recorded` without adding
  another observation or changing the original grouping, time or metadata.
  Changed source/partial/branch/tag metadata produces a warning and retains
  the first values: a retry cannot upgrade a partial run to full. When only
  some reports are already present, the whole import fails with an overlap
  error rather than double-counting old reports or dropping unseen ones.
- Grouping is first-recorded, not regrouped on replay. Importing A then B
  separately, then A+B, keeps two runs; fresh A+B makes one. Importing A
  then A+B conflicts; A+B then A is an already-present subset. Subset or
  spanning-group replays check membership only: per-report contributions
  cannot be recovered from aggregate results, so aggregate equivalence is
  not independently verified. Exact-group replays with disagreeing stored
  results fail. Report fingerprints supplied by a transported database are
  producer claims, not proof a test executed or unavailable XML was genuine.
- `ingest` imports reports; it does not infer new executions. The CI helper
  `scripts/record-history.sh` follows these same import rules, so separate CI
  executions with identical report bytes and commit can be retries. First-class
  `run nextest` records every actual execution separately, even with identical
  reports. Older history has no raw report fingerprints and stays unkeyed;
  its first new keyed import may coexist with an equivalent legacy observation.
- Stores opened for writing and exported snapshots use schema 5, even for
  unkeyed executions. Read-only opens and snapshot export do not migrate
  their input. Upgrade every importer, runner and trusted
  publisher sharing the store or history ref together; older binaries reject
  newer stores. Do not
  downgrade or strip the membership table to make an old publisher accept it.
  A stale or rolled-back CI pin can leave the job green while printing
  ⚠ warnings containing `unsupported snapshot schema version` and
  `History was not published`: history publication stops and that run's artifact
  is not recovered automatically. Move the pin forward to a schema-5-capable release; do not
  downgrade the database.
  Membership survives snapshot export, merge and restore. Publication keeps
  the remote destination's grouping/provenance; restore into an existing store
  keeps the local destination's. Unlike strict local imports, transport excludes
  an entire partially overlapping incoming group, warns with its opaque history
  ID, and still transfers unrelated runs. It neither splits aggregates nor counts
  overlapping reports twice. After publication, that group's unseen reports
  remain only in the source artifact, not the canonical snapshot; there is no
  automatic recovery. After restore, excluded remote groups remain in the source
  rather than the local store. A retained local grouping can still be excluded
  by the next publication: successful restore does not reconcile that conflict.
  [Durable history](docs/design/durable-history.md#schema-5-report-memberships)
  describes the typed outcomes and limitations.
- greenshift's messages show what was given on the command line (paths,
  and in usage errors a rejected value or an unexpected argument or
  subcommand) with control characters and backslashes escaped as in
  Rust's `{:?}`, so a file name cannot send terminal escape sequences or
  break a line.
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

Known producers: cargo-nextest, vaelii's test runner and `greenshift run
lein`'s reporter. Anything that writes
the common `<testsuites>/<testsuite>/<testcase>` shape should work.

## Select

```
greenshift select [--db PATH] [--source NAME]... [--min-runs N]
                  [--coverage N] [--confidence N]
                  [--estimator wilson|empirical|smoothed]
```

`select` reads the store and writes nothing. It sorts the tests of your
latest full run into *rich* tests, which together have caught every failure
on record, and *poor* tests, which can run after them:

- **Full runs.** Runs are taken oldest first. A run ingested with
  `--partial` is never full. Of the rest, the first is full; after that
  a run is full when it has at least 90% as many results as the largest of
  the three full runs before it, so a short run (say, of only the selected
  tests) is not full either. With `--source`, given once or more, only runs from
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
  (default 120) that passed, failed or errored, in any run read; skips do
  not count. The rest are *new* and not listed.
- **Median.** A test's duration is the median of its last 5 non-skipped
  results that have one, in any run read; a test with none gets the median of the other
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

By default rich is every picked test. `--coverage N` and `--confidence N`
(percentages from 0 to 100) end rich early, at the first picked test whose
coverage or confidence is at least N; the rest of the picked tests are then
treated like any other non-rich test: poor with enough results, otherwise
new. With neither flag, rich behaves as with `--coverage 100`. With both,
rich ends at the first test where both hold, so it is never shorter than
either flag alone would make it. If a threshold is never reached, rich is
all the picked tests and stderr names the threshold and the highest value
reached.

Exit status:

- 0: the selection was printed. That can be nothing at all: with no
  failing full run there is no rich test, and until a test has
  `--min-runs` results it is new, so a young history lists nothing.
- 1: an error, such as a `--db` file that is not a greenshift store.
- 2: no full run from the chosen sources, or no store at all. stderr says
  `no history: run the full suite` and nothing is printed to stdout.

## Nextest filter

```
greenshift nextest-filter --inventory FILE [--rich | --poor]
                          [--db PATH] [--source NAME]...
                          [--min-runs N] [--coverage N] [--confidence N]
                          [--estimator wilson|empirical|smoothed]
```

`nextest-filter` takes `select`'s options, works out `select`'s lines
from the inventory's tests (see **Inventory** below), and prints one
[filterset](https://nexte.st/docs/filtersets/) for `cargo nextest run -E`. `--inventory` is the current
`cargo nextest list --message-format json` output, full or
`--list-type binaries-only`, as a file or `-` for stdin:

```
cargo nextest list --list-type binaries-only --message-format json \
    > inventory.json
cargo nextest run -E "$(greenshift nextest-filter --source nextest \
    --inventory inventory.json)"
```

- **Terms.** In nextest's JUnit, a testcase's `classname` is the test
  binary's id and its `name` the test's name, so each stored test becomes
  `(binary_id(=<suite>) & test(=<name>))`. A test stored with an empty
  suite becomes `test(=<name>)`, which matches that name in every binary.
- **Inventory.** Tests whose binary id is not in the inventory are left
  out: nextest refuses a whole filterset in which a `binary_id(=...)`
  matches no binary, as one does once a binary is renamed or removed.
  They are left out before the cover is picked, so a failure only a gone
  test would have been picked for gets a pick of its own from the tests
  left that also failed in it: one of them, not every one. A full
  listing also names each listed binary's tests, and a stored test
  missing from its binary is left out the same
  way; a binaries-only listing, or a binary a full listing skipped,
  keeps every test of the binary. Which runs count as full is still
  decided on the whole runs, and a run in which only tests left out
  failed is never counted as green.
  A binaries-only listing ignores `-E`, and a full one keeps binaries it
  filters out (as `skipped`), so neither drops a binary; `-p`,
  `--exclude` and the target-selection flags (`--lib`, `--test` and so
  on) do, so leave them out when making the inventory.
  The option is required, and an inventory that cannot be read or is not
  such a listing is an error (exit 1).
- **`--poor`** prints the terms of the poor tests joined by ` | `, or
  `none()` when there are none.
- **`--rich`**, the default, prints every test but the poor ones:
  `not (<poor terms>)`, or `all()` when there are none. So tests that are
  new, or not in the store at all, run with the rich tests.
- **Escaping.** Within a term, `)`, `,`, `\` and `/` are escaped with a
  backslash, newline, carriage return and tab as `\n`, `\r` and `\t`, and
  other control characters as `\u{HEX}`, so the filterset is one line.
  Everything else, spaces and `(`, `|`, `&` and `::` included, is written
  as is: nextest reads a matcher up to the first unescaped `)` or `,`.

- **Size.** Linux limits one command-line argument to 128 KiB
  (MAX_ARG_STRLEN, 131,072 bytes with its terminating NUL). A term is
  about 92 bytes (greenshift's own 193 tests average 91.5), so
  `-E "$(...)"` fails with `argument list too long` from about
  131,072 / 92 ≈ 1,400 poor tests on. Past that, the filterset has to
  reach nextest another way than as one argument, such as from a file or
  as a profile's `default-filter` in `.config/nextest.toml`.

With no history (where `select` exits 2) there is no poor test, so
`--rich` prints `all()` and `--poor` prints `none()`, and a fresh repo
runs every test; stderr still says `no history: run the full suite`, and
the exit status is 0. Otherwise the exit status, and stderr for a
threshold never reached, are as for `select`.

## Plan

```
greenshift plan --inventory FILE [--db PATH] [--source NAME]...
                [--min-runs N] [--coverage N] [--confidence N]
                [--estimator wilson|empirical|smoothed]
```

`plan` is `nextest-filter` for any test runner. It takes `select`'s
options, works out `select`'s lines from the inventory's tests alone,
and splits the tests that exist now into a rich group, to run first, and
a poor group, to run after.
`--inventory` lists those tests, one JSON object per line, as a file or
`-` for stdin:

```
{"suite":"vaelii.core-test","name":"parses-a-rule"}
{"suite":"vaelii.core-test","name":"rejects a bent, (odd) name"}
```

It prints one line of JSON:

```
{"history":"ok","rich":[{"suite":"…","name":"…"},…],"poor":[{"suite":"…","name":"…"},…]}
```

- **History.** `history` comes first, as it says how to read the groups.
  It is `ok` when the store was read, with or without runs, or was
  missing and has just been created empty (with its directory, as
  `ingest` would). It is `error` when the store could not be used: it
  was missing and could not be created, or it exists but cannot be read,
  is corrupt, or is not a greenshift store. An existing store is only
  read, never migrated or written.
- **Tests.** A test is its `suite` and `name`, both required strings
  (the suite may be empty), compared exactly with the store's suite and
  name as ingested. Other members are ignored, blank lines are skipped,
  and a test listed twice is planned once. A member repeated within one
  object takes its last value.
- **Groups.** Every inventory test is in exactly one group. Poor is the
  inventory tests on `select`'s poor lines, in `select`'s order. Rich is
  every other inventory test: first `select`'s rich tests in pick order,
  then the rest in inventory order. So tests the store has never seen, and
  those with fewer than `--min-runs` results (`select`'s *new* tests), are
  rich. Stored tests that are not in the inventory are left out, and
  before the cover is picked: when a failing run's only rich witness is
  gone, the cover picks one of the inventory tests that also failed in
  it instead, not every one of them.
  Which runs count as full is still decided on the whole runs, so a run
  cut short never counts as full because the gone tests are left out.
  A run in which only gone tests failed is never counted as green.
- **Names.** Suite and name are written as JSON strings of the raw text:
  JSON's own escapes only (`\"`, `\\`, `\t`, `\u001b` and so on), none of
  `select`'s display escaping, so they read back exactly as stored. Every
  control character is escaped, DEL and C1 (`\u007f`, `\u009b`) too, and
  so is every character that does not print: format characters such as
  bidi controls (`\u202e`) and zero-width spaces (`\u200b`), the line
  and paragraph separators (`\u2028`, `\u2029`), spaces other than
  U+0020 (`\u00a0`), and private-use and unassigned code points, beyond
  U+FFFF as a surrogate pair. Commas, parentheses, slashes and other
  non-ASCII text are written as they are.

With no history (where `select` exits 2) there is no poor test, so every
inventory test is rich; stderr still says `no history: run the full
suite`. With `history` `error` every inventory test is rich too, and
stderr names the error instead, so a store greenshift cannot use slows
the tests down but never stops them. An empty inventory prints
`{"history":"ok","rich":[],"poor":[]}`.

Exit status:

- 0: the plan was printed, with or without history, and whether
  `history` is `ok` or `error`. stderr names any store error, and any
  `--coverage` or `--confidence` threshold never reached, as for
  `select`.
- 1: the inventory could not be used, and nothing is printed to stdout:
  it cannot be read, or a line of it is not a JSON object with string
  `suite` and `name` (stderr names the line). No line is skipped, as a
  skipped test would be in neither group.
- 2: a usage error, such as no `--inventory`.

## Run nextest

```
greenshift run nextest [--db PATH] [--source NAME] [--commit SHA]
                       [--branch NAME | --tag NAME] [--min-runs N]
                       [--coverage N] [--confidence N]
                       [--estimator wilson|empirical|smoothed]
                       [--max-filterset-bytes BYTES] [-- NEXTEST_ARGS...]
```

`run nextest` lists the tests with cargo-nextest, plans them as `plan`
does, runs the rich group and then the poor group, and records both
groups' results in the store as one run:

```
greenshift run nextest --commit "$(git rev-parse HEAD)" -- --no-fail-fast
```

- **List.** `cargo nextest list --message-format json` gives the tests:
  each listed binary's testcases that its filters select. A test's suite
  is its binary id and its name the test's name, as in nextest's JUnit.
- **Plan.** The tests are split into rich and poor as `plan` splits an
  inventory, with `select`'s options. Only runs from `--source` are read
  (default `nextest`), and the run is recorded as from it. A missing
  store is created empty, so every test is rich and stderr says
  `no history: run the full suite`. A store that cannot be read is named
  on stderr, followed by `greenshift: running every test as rich`, and
  every test is rich.
- **Run.** Each group runs as `cargo nextest run -E <filterset>`, the
  filterset naming the group's tests one by one, rich first. The poor
  group runs whether or not the rich group failed. An empty group does
  not run; with no test in either, the rich group runs with `none()`, so
  nextest's `--no-tests` decides the outcome, and nothing is recorded.
- **Batches.** A group whose filterset is longer than
  `--max-filterset-bytes` (default 100,000) runs as several
  `cargo nextest run`s, each naming the next tests in the group's order
  that fit. Every one of them runs, whether or not one before it
  failed. A single test whose term is longer still runs alone. The
  default keeps each `-E` under Linux's 131,072-byte limit on one
  argument (see **Size** under Nextest filter).
- **JUnit.** Each `cargo nextest run` writes JUnit to a file of its own
  in a scratch directory that greenshift removes at the end. greenshift
  sets the file in a tool config file (`--tool-config-file`), which
  nextest ranks below `.config/nextest.toml`. The runs use the profile
  `greenshift`, which inherits the repository's `default` profile; a
  `--profile` among NEXTEST_ARGS, or else a nonempty `NEXTEST_PROFILE`,
  is used instead, and then works only if `.config/nextest.toml` does
  not set that profile's `junit.path`.
- **Record.** Every JUnit file written is ingested as one run, as
  `ingest` records it, with `--commit` if given, and the branch or tag
  as `ingest` finds it: `--branch` or `--tag`, else the CI environment.
  When a run wrote no JUnit, the files that exist are recorded as a
  partial run, and stderr names each missing file. When none exist,
  nothing is recorded. A failure to record is named on stderr and does
  not change the exit status.
- **NEXTEST_ARGS**, after `--`, go to `cargo nextest run`, after
  greenshift's own arguments. They go to `cargo nextest list` as well,
  except runner and reporter options that only `run` takes (such as
  `--no-fail-fast`, `-j` and `--retries`). Options that choose tests,
  `-E` and `--partition`, go to `list` only: the listing applies them,
  and a second `-E` on `run` would add the tests it matches to the
  group's. Test name filters and everything after a further `--` go to
  both.

Exit status:

- The exit status of the first `cargo nextest run` that failed, in the
  order they ran, or 0 when every one succeeded. One that could not start
  counts as 1, and one a signal ended as 128 plus the signal.
- The exit status of `cargo nextest list` when it fails, with nothing run
  or recorded.
- 1 for an error before any test runs, such as listing output that
  greenshift cannot read; 2 for a usage error.

## Run lein

```
greenshift run lein [--db PATH] [--source NAME] [--commit SHA]
                    [--branch NAME | --tag NAME] [--min-runs N]
                    [--coverage N] [--confidence N]
                    [--estimator wilson|empirical|smoothed]
                    [--max-only-bytes BYTES] [-- LEIN_TEST_ARGS...]
```

`run lein` does for a Leiningen project what `run nextest` does for a
Rust one: it lists the tests, plans them as `plan` does, runs the rich
group and then the poor group, and records both groups' results as one
run, one result per deftest. Run it in the project's root:

```
greenshift run lein --commit "$(git rev-parse HEAD)"
```

- **List.** One `lein test` JVM lists the tests, through lein's own
  selection: `lein update-in :injections conj '(load-file "...")' --
  test LEIN_TEST_ARGS`, where the injected file makes
  `clojure.test/run-tests` write each var lein selected and run none. So
  the listing is exactly what `lein test` would run: its discovery, its
  `:test-selectors` (`:default` with no arguments), namespace metadata
  and tests that macros define (`defspec`, your own). A test's suite is
  its namespace and its name the var's. stderr says how many it listed
  and how long that took. The listing compiles the whole test tree once,
  and the group runs compile it about once more between them.
- **Plan.** As `run nextest` plans, with `--source` defaulting to `lein`.
  With `--source` the run is recorded as from that source.
- **Run.** Each group runs in one more JVM, the rich group first, as
  `lein update-in :injections conj '(load-file "...")' -- test :greenshift`.
  The `:greenshift` selector, which `init lein` adds to project.clj,
  reads the group's names from a file greenshift writes (one `ns/var`
  per line, the path in `-Dgreenshift.names`) and loads only the
  namespaces it names. A project without the selector (or with a
  `:test-selectors` that is computed, `~(...)`) runs each group as
  `lein test :only ns/var ...` instead, in batches of at most
  `--max-only-bytes` of names (default 100,000), and stderr says so. The
  poor group runs whether or not the rich group failed.
- **JUnit.** The injected file also installs greenshift's JUnit reporter
  (ported from vaelii-lacuna's run-vars reporter) and runs the group's
  namespaces in the order the group first names them. One testcase per
  var, with a `<failure>` per failed assertion and an `<error>` per
  exception. An assertion in a fixture, and a fixture that throws, go on
  a testcase named `fixtures` in the namespace; tests the group named
  that never ran (after a fixture error, say) are skipped. The file is
  written before lein exits, or by a shutdown hook when a test exits the
  JVM; a JVM that fails before any test runs (a namespace that does not
  compile) writes none, and the run is recorded as partial. Every hook
  chains to what was there, so a project's own `clojure.test` hooks and
  lein's output stay as they are; a `report` method that a test
  namespace defines as it loads is chained to as well, when the run
  starts.
- **Record.** As `run nextest` records.
- **LEIN_TEST_ARGS**, after `--`, go to the listing's `lein test` only:
  selectors such as `:slow` and namespaces. The runs get only
  `:greenshift` (or `:only` and the names): lein runs a test any one
  selector matches, so a selector given to a run as well would add its
  tests to the group's.
- **Reflection.** lein writes the test form to a file without metadata
  unless project.clj sets `:preserve-eval-meta true` (lein 2.10 and
  later), so with `*warn-on-reflection*` on and without that key, the
  selector's type hints are lost and lein prints reflection warnings for
  it. greenshift's injected files never warn.

Exit status: as for `run nextest`, lein's exit status in place of
nextest's (lein exits 1 when any test fails).

## Set up a repo

```
greenshift init nextest --release TAG --sha256 HEX [--ci github|forgejo]
                        [--runs-on LABEL] [--force]
```

For example, to pin v0.2.0 (its `.sha256` file gives this checksum):

```
greenshift init nextest --release v0.2.0 \
  --sha256 36a30034902f29885c9763d868aae1ea773ca79ddc710465c025a9f4c524074b
```

That example pins a schema-4 release. Before writing or publishing schema-5
history, upgrade all its writers and trusted publishers
to a schema-5-capable release. Regenerate installed workflows with that
release's independently checked checksum and `--force`. Init validates the
tag and checksum format, not the downloaded binary's schema support. Both
jobs use the same pin; do not substitute a publisher built by the test job
or strip report memberships to make an older publisher accept the database.

`init nextest` sets a cargo-nextest repository up to run `greenshift run
nextest` in CI with [durable history](docs/design/durable-history.md), in
one step. Commit what it writes as is:

- **The workflow**, `.github/workflows/greenshift.yml` or
  `.forgejo/workflows/greenshift.yml`: a test job that restores the
  history, runs the tests and uploads the history as a data artifact, and
  a separate publisher that publishes it for pushes to the default branch
  only. `--ci` picks the forge; without it, the one of `.github` and
  `.forgejo` the repository has. `--runs-on` names the runner label
  (default `ubuntu-latest` on GitHub, `docker` on Forgejo), which must be
  an x86_64 Linux runner: releases ship only for
  `x86_64-unknown-linux-musl`.
- **The pin.** The workflow installs the release `--release` names (newer
  than v0.1.0, which predates `run nextest`) and refuses it, failing the
  job, unless it matches `--sha256`. Neither has a default: take the
  checksum from the release's `.sha256` file (see **Install a release**)
  and check it independently.

That is all it writes. `run nextest` has nextest send JUnit to files of
its own (see **JUnit** under Run nextest), so it needs no stanza in
`.config/nextest.toml`, and `init` neither reads nor changes that file.
Running `greenshift ingest` on nextest's own JUnit output still needs the
stanza shown under Ingest.

On Forgejo the workflow is **not isolated**: stock Forgejo gives every
step, the tests included, a token that can write the repository, and
ignores `permissions` (see the
[Forgejo capability gate](docs/design/durable-history.md#forgejo-capability-gate)).
The file says so at the top, and `init` says so when it runs. Forgejo
runs the workflows of only the first of `.forgejo/workflows`,
`.gitea/workflows` and `.github/workflows` there is, so `init` also says
when writing `.forgejo/workflows` would stop the others running.

The workflow publishes the history to `refs/greenshift/history`, whole
and never pruned, where anyone who can read the repository can read it:
anyone at all, if the repository is public (see **Publication audience**
below). The generated file says so too.

It prints what it wrote. A second run with the same options changes
nothing and says so. A generated file that exists and differs is left
alone, and then nothing at all is written, unless `--force` is given.
To pin a new release, run it again with the new `--release`, `--sha256`
and `--force`.

Exit status: 0 when the repository is set up, 1 for a file left alone,
one reached through a symlink (never followed or replaced), or one that
cannot be read or written, with nothing written, and 2 for a usage
error, such as a missing or malformed `--release` or `--sha256`.

## Set up a lein project

```
greenshift init lein [--force]
                     [--ci [github] --release TAG --sha256 HEX
                      [--runs-on LABEL] [--java-version VERSION]]
```

`init lein` sets a Leiningen project up for `greenshift run lein`. Run it
in the project's root and commit what it changes:

- **project.clj.** It adds the `:greenshift` entry to the literal
  `:test-selectors` map of the `defproject`, as the map's last entry at
  its keys' column, with a comment saying what it is; every other byte
  of the file, comments and layout included, stays as it was. With no
  `:test-selectors`, it adds `:test-selectors {:greenshift ...}` after
  the last option. With no `:default` selector, plain `lein test` still
  runs every test: it never reads the entry. An identical entry is left
  as it is; a different one is a conflict (exit 1, nothing written)
  unless `--force`, which replaces its value. A computed `:test-selectors`
  (`~(...)`) is refused (exit 1); `run lein` then uses `:only`.
- **The workflow**, only with `--ci`: `.github/workflows/greenshift.yml`
  (GitHub only, for now), a workflow of its own that runs `greenshift run
  lein` with [durable history](docs/design/durable-history.md), in the
  shape `init nextest` writes, with Temurin (`--java-version`, default
  25), lein 2.12.0 and a cache of `~/.m2`. It is separate from the
  project's own test workflow, which it leaves alone, and runs on pushes
  and by hand, not on pull requests. `--release` and `--sha256` pin the
  greenshift release as for `init nextest`, and are required with
  `--ci`.

Nothing else: no reporter in the repository, no profile and nothing in
`~/.lein`. It prints what it changed; a second run changes nothing and
says so. A file reached through a symlink is never read or written, and
a conflict in either file means neither is written.

Exit status: 0 when the project is set up, 1 for a conflict, a computed
`:test-selectors`, no project.clj, or a file that cannot be read or
written, with nothing written, and 2 for a usage error.

## Durable CI history

Opt-in git-ref snapshots, stock cache integration and a separate trusted publisher
are described in [durable history](docs/design/durable-history.md), including the
GitHub workflow reference and the unresolved stock Forgejo token-isolation gate.
`greenshift history --help` lists the storage commands. Existing local runs keep
their current behavior unless `--history-remote` is supplied. Restore combines
canonical and local history rather than discarding unpublished local runs.

**Publication audience:** snapshots include the exact JUnit file paths stored in
`run_files`, which may contain workstation usernames in absolute paths. Everyone
with access to the destination repository can read that metadata. Use relative
JUnit paths at `greenshift ingest` when absolute paths should not be published;
`run nextest` records the absolute paths of its own JUnit files, in a
fresh directory under `$TMPDIR` (or `/tmp`), so they carry a user name only
if `$TMPDIR` does.

Ordinary history storage errors warn without failing tests; malformed history arguments
or publish-policy refusals exit 2. A second CAS rejection does not automatically
recover that run's new results; durable delivery remains separate work. Ctrl-C
during history transport terminates its Git helpers and returns 130.

**Resource policy:** greenshift imposes no fixed row, byte, field-length or
expansion cap, so large/frequently tested repositories need no size configuration. An
untrusted history artifact can therefore exhaust the publisher's memory, disk
or CPU, even when the test job has a read-only token. Structural validation and
immutable shared test keys do not prevent resource exhaustion. See the
[resource policy](docs/design/durable-history.md#resource-policy) for the boundary.

## Building

```
cargo build --release
```

The test suite needs [cargo-nextest](https://nexte.st/) installed: some
tests run it on a scratch project. Run it with `cargo nextest run`, and
the doctests with `cargo test --doc`. The tests of `run lein`
(`tests/run_lein.rs`) need a JDK and `lein` on PATH, and skip without
them unless `GREENSHIFT_REQUIRE_LEIN` is set, as CI sets it.

SQLite is bundled, so release builds for `x86_64-unknown-linux-musl` are
fully static.

## Install a release

Each release tag (such as `v0.1.0`) has a static Linux binary and its
SHA-256 checksum:

```
https://github.com/paceheart/greenshift/releases/download/<tag>/greenshift-<tag>-x86_64-unknown-linux-musl
https://github.com/paceheart/greenshift/releases/download/<tag>/greenshift-<tag>-x86_64-unknown-linux-musl.sha256
```

Download both into one directory, then check the binary against the
checksum before you run it:

```
tag=v0.1.0
base="https://github.com/paceheart/greenshift/releases/download/$tag"
curl -fLO "$base/greenshift-$tag-x86_64-unknown-linux-musl"
curl -fLO "$base/greenshift-$tag-x86_64-unknown-linux-musl.sha256"
sha256sum -c "greenshift-$tag-x86_64-unknown-linux-musl.sha256" &&
    chmod +x "greenshift-$tag-x86_64-unknown-linux-musl"
```

`sha256sum -c` prints `OK` and exits 0 only when the file matches. To pin
a version, record the tag and the checksum and compare against those.

## License

MIT; see [LICENSE](LICENSE).
