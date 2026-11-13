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
cargo-nextest's JUnit output turned on (`[profile.default.junit]` in
`.config/nextest.toml`):

```
cargo nextest run
greenshift ingest target/nextest/default/junit.xml \
    --commit "$(git rev-parse HEAD)" --source nextest
```

- `--db` defaults to `.greenshift/history.sqlite`; the directory is created.
- `--commit` is optional. `--source` names the producer or CI job
  (default `junit`).
- A test is identified by suite and name. The suite is the testcase's
  `classname`, or the enclosing `<testsuite>` name when there is none.
- Outcomes are `passed`, `failed`, `error` and `skipped`; durations are
  stored in whole milliseconds. Failure text and test output are not stored.
- If any file is unreadable or malformed, greenshift exits 1 and writes
  nothing.

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
