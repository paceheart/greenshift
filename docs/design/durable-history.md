<!-- design-meta: status=implemented-repair-review-pending; last-updated=2026-11-21; phase=implementation -->
# Durable history

`refs/greenshift/history` is the canonical full-database snapshot of published
history, not a promise that every source observation was admitted. Each publication
replaces a parentless commit containing exactly `history.sqlite.gz`. Reachable
history is bounded to the current snapshot. Replaced objects remain subject to
the forge's garbage collection and retention; this does not promise bounded
physical server storage or immediate deletion.

## Interfaces

```
greenshift history cache-key --remote URL
greenshift history restore --remote URL [--db PATH] [--cache-dir DIRECTORY]
greenshift history publish --remote URL [--db PATH] [--cache-dir DIRECTORY]
                           [--default-branch NAME]
greenshift run nextest --history-remote URL --history-artifact PATH
                      [--history-cache-dir DIRECTORY] [other run options]
```

`run` is opt-in: existing local runs are unchanged. Restore merges fetched history
with an existing local store, preserving its destination runs and admitting
nonconflicting incoming observations under the membership rules below. It happens before test
listing; unavailable canonical history runs all tests as rich. Recording remains
best-effort and the artifact is written after recording even when tests fail.
Storage errors emit `⚠` and preserve the test result. Explicit history commands
also warn and return zero on storage failure. Invalid arguments/configuration or
publication-policy refusals exit 2; cancellation exits 130. Successful publish
prints the new commit identity even if its optional cache copy cannot be saved;
a failed publish prints no identity.

An existing local database is updated in place under an SQLite write transaction,
so a completed local write cannot be discarded by replacing its pathname with a
stale copy. Creating a missing local database does not overwrite a concurrent
creator; that database is merged transactionally instead. This relies on ordinary
SQLite writer coordination, not protection against unrelated direct filesystem
replacement or deletion of the selected database.

On Unix, authentication is supplied through `GREENSHIFT_HISTORY_TOKEN` for HTTPS
only; authenticated HTTP and redirects are refused. Unauthenticated HTTP and
local paths/file URLs are also supported; SSH/custom helpers are not. Token
authentication on other platforms reports unsupported without breaking local
ingestion/selection. URL-embedded credentials are refused. Git runs in its own
temporary bare repository, with inherited Git configuration removed (except
CA trust: the variables `GIT_SSL_CAINFO`/`GIT_SSL_CAPATH`, and `http.sslCAInfo`/
`http.sslCAPath` from the caller's global or system gitconfig, never a
repository's own config), hooks disabled, no
checkout, and supervised operation duration/diagnostics. History payload
stdin/stdout is not size-capped. Tokens do not enter command arguments or
diagnostics.

The wire protocol currently supports SHA-1 Git repositories; a SHA-256 remote
history identity is refused explicitly rather than treated as a usable snapshot.
The binary installs a process-wide Ctrl-C handler. During active history transport,
cancellation terminates its isolated Git process group and returns 130. Outside
transport, Ctrl-C keeps normal termination behavior. Library transports install no signal
handler. Test-runner listing/build/run children do not inherit
`GREENSHIFT_HISTORY_TOKEN`.

## Published data and audience

Snapshots preserve destination history and admitted incoming observations,
including `run_files.path` exactly as ingested. Keyed replays and conflicting
incoming groups follow the schema-5 rules below; not every source observation is
necessarily transferred. Absolute JUnit paths can contain workstation usernames or other
host-specific information; anyone with repository access can read that metadata
from the durable ref. Audit the destination audience before publishing. Ingest
relative JUnit paths when absolute paths should not be shared. The snapshot does
not redact paths or silently change durable run identities.

## Stock cache contract

`cache-key` prints `greenshift-history-COMMIT` or `greenshift-history-empty`.
Only stock `actions/cache/restore` and `actions/cache/save` speak the cache
protocol. The directory contains `COMMIT/history.sqlite.gz` and `COMMIT/commit`.
The cached raw commit is hashed against the current canonical identity, then its
tree is checked against the compressed payload's Git blob identity. Naming a
directory after a commit is not sufficient to authenticate its contents. Invalid
or absent cache data falls back to fetching the canonical ref. No prefix restore
key can make an older cache authoritative.

The reference [GitHub reusable workflow](../../examples/durable-history.github.yml)
is not installed by this change. A caller must choose a published release that
contains these commands and pin its independently verified SHA-256.
`greenshift init nextest` (#21) generates a standalone workflow from this one,
with the release and SHA-256 given on its command line and a stricter install
step (the download is moved into place only once `sha256sum --check --strict`
passes), and a Forgejo variant that states the capability gate below instead
of claiming isolation.
Per-branch saves/fold-in, multi-invocation keys and cache-only mode are
separate work.

Reusable workflows cannot elevate their caller's permissions. A caller must
grant `contents:write` to the calling job; the called **test** job explicitly
downgrades its token to read-only. For example, after copying the reference:

```yaml
name: Tests
on: [push, pull_request]
jobs:
  test-history:
    permissions:
      contents: write
    uses: ./.github/workflows/history.yml
    with:
      release: v0.1.0 # Replace with the chosen release containing history commands.
      sha256: REPLACE_WITH_INDEPENDENTLY_VERIFIED_RELEASE_SHA256
```

These placeholders must be filled before installation; they do not claim an
existing release already contains the feature. Fork PRs run tests only. Missing
history artifacts warn and skip publication/cache save rather than fail an
otherwise successful workflow.

## Publisher trust boundary

The GitHub test job has `contents:read`, checkout does not persist credentials,
and the publisher receives `contents:write` on a **fresh hosted runner**. It never
checks out or executes test output; only a checksummed release binary parses a
single data artifact. Publication is only for pushes to the configured default
branch, never pull requests (including `pull_request_target`). The CLI repeats
that policy for CI invocations and requires `--default-branch` there.

The parser accepts only known supported schema versions/application id, disallows extra
views/triggers/tables/indexes, verifies integrity and foreign keys, checks types,
IDs/outcomes/durations/ref exclusivity. Artifact connections are read-only and defensive with
untrusted schemas disabled. Export constructs a new canonical database using
bound values; no input SQL is executed as SQL. Known older schemas are validated
against their own exact trusted migration-prefix schema before data is projected
into a fresh current database. Newer unknown versions are refused without
altering the source or canonical ref. The local restore destination is validated
on its write-reserved connection before any rewrite. Export replacement and
existing-store transaction commits are atomic.

Commit strings from existing history remain historical data, including
arbitrary strings written before commit validation existed. New ingest input
still uses `CommitId` validation. Historical commit strings are copied using
bound SQL values, never used as Git command arguments or shell input.

## Resource policy

History is deliberately uncapped by greenshift: there is no application-defined SQLite/file/logical-page,
row-count, text/path-length, compressed/decompressed payload or application
allocation limit, and no size knob to tune for large or frequently tested
repositories. Old history is not pruned. These are not forge/platform limits.

This leaves a resource-exhaustion exposure. A compromised test dependency can
rewrite the data artifact even when its job token is read-only; structurally
valid or compressed data can consume the trusted publisher's memory, disk or
CPU. Commit/blob authentication identifies data, not its resource cost. The
separate publisher prevents executing test output but is not a resource sandbox.
This is a documented threat scenario, not a report of an observed incident.

The in-memory snapshot uses an immutable local test dictionary. Results hold
typed dictionary-local IDs rather than copying suite/name text per result, and
merging remaps IDs by the complete semantic test key before identity comparison.
Retained text, binary file paths and completed collections expose read-only APIs;
construction/remapping uses explicit mutable builders. The dictionary does not
globally intern every equal string, change the SQLite wire schema or guarantee
that arbitrarily large history fits finite host resources.

Protocol metadata and operational controls remain: cached raw commit metadata is
limited to 16 KiB, each Git operation has a 60-second supervision timeout, Git
stderr has a 1 MiB supervision limit, and SQLite lock waits time out after five
seconds. None is a lifetime-history row/blob quota. Structural/schema/ref/digest
checks, owned Git-process cleanup, and atomic staged replacement still apply.
Validation/build failures before the push leave the canonical ref unchanged;
ordinary errors clean up owned temporary files. Optional cache I/O failure after
a successful push does not undo publication. Resource exhaustion that aborts/kills the process is not a
guarantee of graceful error reporting or destructor cleanup. Parentless refs do
not promise provider-side garbage collection or bounded physical forge storage.

SQLite migration 4 gives each run a durable 128-bit identity. New runs use SQLite
random bytes. Legacy identities hash typed run metadata, original local id,
ordered files and results, so separately migrated copies of one legacy DB agree.
Local id preserves repeated identical legacy invocations. Already-diverged legacy
stores cannot recover whether identical historical records were independently
recorded or shared; that information was never recorded. Same identity with
conflicting contents is rejected rather than silently replacing history. Merged
runs are numbered in `(ingested_at, history_id)` order: this is deterministic
recorded-time ordering, not proof of actual execution chronology. Timestamp ties
do not merge independent run identities.

Each publish fetches and validates current history, checks shared durable
identities and then merges by report membership as described below,
and uses an exact `--force-with-lease` compare-and-swap. Rejection refetches,
remerges and retries once. A second rejection warns. Workflow concurrency reduces
collisions but is not the correctness lock, and pending workflow cancellation is
not a promise that every pushed commit receives a run. Newly produced results
that lose both attempts are best-effort: no later publisher automatically reads
the failed run's artifact. It can be replayed manually only while retained (one
day in the reference workflow). Durable pending delivery/replay is separate work;
already-published canonical history remains protected by CAS.

## Schema 5 report memberships

Migration 5 adds immutable report memberships: a versioned fingerprint of exact
report bytes and the optional normalized textual commit namespace. Paths, input
order, timestamps and source/partial/ref metadata are not import identity. Legacy
runs remain unkeyed because their original report bytes are unavailable. Genuine
executions are also unkeyed and stay distinct, even when their reports match.

Local `ingest` is strict and atomic: none seen records one aggregate, all seen
acknowledges a replay, and some seen plus unseen reports fails without inserting
anything. Repeated bytes within a fresh bundle collapse before aggregation.
First grouping/provenance wins; metadata-only retries never upgrade a partial
run. Exact-group result disagreement is an error. Subset/spanning all-seen
replays establish membership only, not aggregate-result equivalence.

Transport is destination-authoritative and best-effort. Publication's destination
is canonical remote history; restore's destination is the existing local store.
Every shared retained run identity is compared in full **before** deduplication or
group exclusion. Unequal shared identities, malformed data and exact-group result
conflicts remain hard errors, leaving the destination/ref unchanged.

If an incoming keyed group partly overlaps destination memberships, the entire
incoming group is excluded. Its files/results/memberships are not split or
relabelled as an unkeyed execution. All destination runs and admissible unrelated
incoming runs remain. Names used only by excluded results are omitted from the
output dictionary; destination-only unused names and unrelated incoming names
remain. Source files are unchanged when the merge output is distinct; existing
local restore intentionally updates its destination transactionally.

For example, remote D{A} and local I{A,B}+U publish D+U, excluding I. Restore may
retain local I and add U without reconciling its grouping. Later publications
continue admitting unrelated runs but exclude I again. In the reverse direction,
restoring remote I{A,B} into local D{A}+U excludes I locally; publishing back keeps
canonical I and admits U, treating D as a membership-only subset replay. Neither
direction double-counts A or reconstructs B from aggregate A+B results.

Snapshot merge returns an immutable `MergeReport`. Reports identify
excluded groups by validated opaque history ID and number of unseen memberships,
plus retained destination groups that partially overlap the source.
`Remote::publish` returns `PublishOutcome`: `oid()` is the committed OID and
`report()` is the **final successful CAS attempt's** report. `Remote::restore`
returns `RestoreOutcome::Missing` or `Restored(MergeReport)`, distinguishing
missing history from a committed restore with limitations.
CLI history commands and the runner's restore path render these limitations.

Publication still emits its OID and keeps CI nonblocking, but warns explicitly
that an excluded group was **not published**: its unseen memberships remain only
in the source artifact, not the canonical snapshot, with no automatic recovery.
Restore warns that excluded groups were **not restored locally** and remain in
the incoming source. It also identifies retained local groups that can be
excluded when publishing back: successful restore did not reconcile them. No
warning dumps suites, test names, paths or raw report contents. This is not a
durable pending-delivery guarantee; eventual recovery is separate work in
[#30](https://github.com/paceheart/greenshift/issues/30).

Every writer-opened store and exported snapshot uses schema 5, even for unkeyed
executions. Read-only opens and export do not migrate their input. Upgrade all
writers/importers/runners and trusted publishers sharing a store/ref together.
Generated workflows use one release/checksum pin for both jobs; regenerating with
`--force` updates them together. A stale or downgraded schema-4 pin can leave CI
green while warning `unsupported snapshot schema version` and
`History was not published`; its publisher emits no OID and that run's artifact
has no automatic recovery. Move forward to compatible binaries, never strip
memberships or downgrade the database to silence the warning. Tag/checksum format
validation cannot establish a chosen binary's schema support.

## Forgejo capability gate

Stock Forgejo runner v13.2.2 exports its automatic repository token to every
step as both `GITHUB_TOKEN` and `FORGEJO_TOKEN`. Stock Forgejo v16.0.3/v17.0.2
grants that token write access outside fork pull requests and ignores workflow
`permissions`. `persist-credentials:false` or only referencing the token in a
publisher step does **not** make its test job read-only. A separate publisher
credential does not revoke the automatic one. This is a platform baseline, not
an additional privilege granted by greenshift, but it does not satisfy this
design's read-only-test-job guarantee.

Consequently no ordinary push-test Forgejo workflow is offered as isolated.
Forge-neutral git transport can be used by authenticated trusted code only with
this limitation understood. Owner reconciliation of the token boundary is
required before claiming Forgejo acceptance. No server/runner configuration or
credentials are changed here. Artifact/cache action compatibility on Forgejo must
be independently tested, not inferred from GitHub's API or action version.

Primary sources:
[runner token export](https://code.forgejo.org/forgejo/runner/src/tag/v13.2.2/act/runner/run_context.go),
[server token permission](https://codeberg.org/forgejo/forgejo/src/tag/v17.0.2/routers/api/v1/permissions/repo_access.go),
[Forgejo compatibility](https://forgejo.org/docs/latest/user/actions/github-actions/).

## Verification boundaries

Local tests cover complete ref round-trips after total cache deletion, stale
snapshot union/replay, malformed cache fallback, tampered artifact/ref unchanged,
legacy identity, independent identical new runs, data bounds and 50k+ rows.
Live GitHub/Forgejo scratch CI, rulesets, fork restoration, actual cache-service
interruption and cleanup listings are separate acceptance receipts; local tests
do not claim those ran.
