# greenshift

greenshift picks which tests to run from what your tests have done before.

It is early. This release records test history from JUnit XML into a
per-repo SQLite store. Choosing tests from that history (`greenshift select`)
is next.

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
  `classname`, or the enclosing `<testsuite>` name when there is none.
- Outcomes are `passed`, `failed`, `error` and `skipped`; durations are
  stored in whole milliseconds. Failure text and test output are not stored.
- greenshift exits 1 and writes nothing if any file is unreadable or not
  UTF-8, is XML that quick-xml rejects, has a root other than `<testsuites>`
  or `<testsuite>`, has content outside the root element, or has a
  `<testcase>` without a name or with a `time` that is not seconds. It does
  not otherwise validate XML well-formedness: the input is expected to come
  from a known producer.

Known producers: cargo-nextest and vaelii's test runner. Anything that writes
the common `<testsuites>/<testsuite>/<testcase>` shape should work.

## Building

```
cargo build --release
```

SQLite is bundled, so release builds for `x86_64-unknown-linux-musl` are
fully static.

## License

MIT; see [LICENSE](LICENSE).
