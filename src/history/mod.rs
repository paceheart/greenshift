//! Durable history snapshots. Git is canonical; a CI cache is an optional copy.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

#[cfg(unix)]
use secrecy::ExposeSecret;
use secrecy::SecretString;
use tempfile::TempDir;

mod outcome;
pub mod snapshot;
pub use outcome::{GroupOverlap, MergeReport, PublishOutcome, RestoreOutcome};

/// The portable history protocol's sole reference.
pub const HISTORY_REF: &str = "refs/greenshift/history";
const BLOB: &str = "history.sqlite.gz";
const TIMEOUT: Duration = Duration::from_secs(60);

/// Inherited git variables that only choose which certificate authorities
/// git trusts. CI runners behind a private CA set them for every job; without
/// them every HTTPS history operation fails verification.
const KEPT_GIT_VARS: [&str; 2] = ["GIT_SSL_CAINFO", "GIT_SSL_CAPATH"];

/// The inherited environment variables removed from every git child.
///
/// Git's inherited configuration can run hooks, redirect object stores, or
/// install remote helpers. Only the isolated transport owns its state, except
/// for the CA trust in `KEPT_GIT_VARS`. Windows environment names are
/// case-insensitive, so there `Git_Dir` is `GIT_DIR` to git and is matched
/// without regard to case.
fn scrubbed_git_vars(
    keys: impl IntoIterator<Item = std::ffi::OsString>,
) -> Vec<std::ffi::OsString> {
    scrubbed_git_vars_with_case(keys, cfg!(windows))
}

fn scrubbed_git_vars_with_case(
    keys: impl IntoIterator<Item = std::ffi::OsString>,
    case_insensitive: bool,
) -> Vec<std::ffi::OsString> {
    keys.into_iter()
        .filter(|key| {
            let key = key.to_string_lossy();
            let key = if case_insensitive {
                key.to_ascii_uppercase()
            } else {
                key.into_owned()
            };
            key.starts_with("GIT_") && !KEPT_GIT_VARS.contains(&key.as_str())
        })
        .collect()
}

/// Failure to transport or validate history. Callers decide whether to warn.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("history operation cancelled")]
    Cancelled,
    #[error("history configuration: {0}")]
    Usage(&'static str),
    #[error("history: {0}")]
    Invalid(&'static str),
    #[error("history I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("history text: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    #[error(transparent)]
    Snapshot(#[from] snapshot::SnapshotError),
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error("history: git operation failed ({status})")]
    Git {
        status: ExitStatus,
        lease_rejected: bool,
    },
}

impl HistoryError {
    pub fn is_cancelled(&self) -> bool {
        matches!(
            self,
            Self::Cancelled | Self::Snapshot(snapshot::SnapshotError::Cancelled)
        )
    }
    /// Configuration mistakes are usage errors, unlike unavailable storage.
    pub fn is_usage(&self) -> bool {
        matches!(self, Self::Usage(_))
    }
}

impl From<&'static str> for HistoryError {
    fn from(message: &'static str) -> Self {
        Self::Invalid(message)
    }
}

impl From<tempfile::PersistError> for HistoryError {
    fn from(error: tempfile::PersistError) -> Self {
        Self::Io(error.error)
    }
}

fn error(cause: impl Into<HistoryError>) -> HistoryError {
    cause.into()
}

/// Validate syntax without filesystem or network access. Local plain paths are
/// passed to Git only after canonicalization by [`Remote::new`].
pub fn validate_remote_argument(url: &str) -> Result<(), HistoryError> {
    let invalid = |message| HistoryError::Usage(message);
    if url.is_empty() || url.starts_with('-') || url.chars().any(char::is_control) {
        return Err(invalid("invalid history remote"));
    }
    let windows_drive = cfg!(windows)
        && url.as_bytes().get(1) == Some(&b':')
        && url
            .as_bytes()
            .get(2)
            .is_some_and(|b| matches!(b, b'/' | b'\\'));
    if url.contains("://")
        && !["https://", "http://", "file://"]
            .iter()
            .any(|prefix| url.starts_with(prefix))
    {
        return Err(invalid("unsupported history remote protocol"));
    }
    if !url.contains("://") && url.contains(':') && !windows_drive {
        return Err(invalid("unsupported history remote syntax"));
    }
    if let Some((_, authority)) = url.split_once("://") {
        if authority
            .split('/')
            .next()
            .unwrap_or_default()
            .contains('@')
        {
            return Err(invalid("history remote must not embed credentials"));
        }
    }
    Ok(())
}

/// One isolated git transport, with no repository checkout or inherited hooks.
pub struct Remote {
    scratch: TempDir,
    url: String,
    token: Option<SecretString>,
    timeout: Duration,
    cancellation: Arc<AtomicBool>,
}

// A git operation owns the entire helper process group, not just its immediate
// process. Early errors and timeout must not leave network transports running.
struct GitChild {
    child: Child,
    complete: bool,
}

impl Drop for GitChild {
    fn drop(&mut self) {
        if !self.complete {
            #[cfg(unix)]
            {
                // SAFETY: this child was spawned in its own process group; the
                // negative pid addresses that group, never our caller's group.
                unsafe {
                    libc::kill(-(self.child.id() as i32), libc::SIGKILL);
                }
            }
            #[cfg(not(unix))]
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Remote {
    /// HTTPS, unauthenticated HTTP, and local git remotes are supported. Tokens are optional,
    /// supplied only through GREENSHIFT_HISTORY_TOKEN, never command arguments.
    pub fn new(url: &str) -> Result<Self, HistoryError> {
        Self::new_cancellable(url, Arc::new(AtomicBool::new(false)))
    }

    /// The caller owns interruption policy. This library never installs signal
    /// handlers; cancellation drops the owned Git process group at the next poll.
    pub fn new_cancellable(url: &str, cancellation: Arc<AtomicBool>) -> Result<Self, HistoryError> {
        if cancellation.load(Ordering::SeqCst) {
            return Err(HistoryError::Cancelled);
        }
        validate_remote_argument(url)?;
        let token = std::env::var("GREENSHIFT_HISTORY_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .map(SecretString::from);
        if token.is_some() && url.starts_with("http://") {
            return Err(HistoryError::Usage(
                "history token authentication requires HTTPS",
            ));
        }
        let url = if !url.contains("://") {
            fs::canonicalize(url)
                .map_err(error)?
                .to_string_lossy()
                .into_owned()
        } else {
            url.to_owned()
        };
        let remote = Self {
            scratch: tempfile::tempdir().map_err(error)?,
            url,
            token,
            timeout: TIMEOUT,
            cancellation,
        };
        remote.git(["init", "--bare", "."], None)?;
        Ok(remote)
    }

    fn git<I, S>(&self, args: I, input: Option<&[u8]>) -> Result<Vec<u8>, HistoryError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.check_cancelled()?;
        self.git_program(OsStr::new("git"), args, input)
    }

    // An explicit process boundary permits fault injection without changing a
    // process-global PATH or exposing a configurable executable to CI data.
    fn git_program<I, S>(
        &self,
        program: &OsStr,
        args: I,
        input: Option<&[u8]>,
    ) -> Result<Vec<u8>, HistoryError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.check_cancelled()?;
        let stdout = tempfile::tempfile().map_err(error)?;
        let stderr = tempfile::tempfile().map_err(error)?;
        // A file-backed stdin removes the pipe write that could block before
        // the child supervision timeout began. History payloads have no size cap.
        let mut stdin = tempfile::tempfile().map_err(error)?;
        if let Some(data) = input {
            stdin.write_all(data).map_err(error)?;
            use std::io::{Seek, SeekFrom};
            stdin.seek(SeekFrom::Start(0)).map_err(error)?;
        }
        let mut cmd = Command::new(program);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        for key in scrubbed_git_vars(std::env::vars_os().map(|(key, _)| key)) {
            cmd.env_remove(key);
        }
        cmd.current_dir(self.scratch.path())
            .env("HOME", self.scratch.path())
            .env("XDG_CONFIG_HOME", self.scratch.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("SSH_ASKPASS")
            .env_remove("SSH_ASKPASS_REQUIRE")
            .env("GIT_AUTHOR_NAME", "greenshift")
            .env("GIT_AUTHOR_EMAIL", "greenshift@localhost")
            .env("GIT_COMMITTER_NAME", "greenshift")
            .env("GIT_COMMITTER_EMAIL", "greenshift@localhost")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "protocol.ext.allow=never",
                "-c",
                "http.followRedirects=false",
            ])
            .args(args)
            .stdin(Stdio::from(stdin))
            .stdout(stdout.try_clone().map_err(error)?)
            .stderr(stderr.try_clone().map_err(error)?);
        if let Some(token) = &self.token {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let askpass = self.scratch.path().join("askpass");
                fs::write(&askpass, "#!/bin/sh\ncase \"$1\" in *Username*) echo x-access-token;; *) printf '%s' \"$GREENSHIFT_HISTORY_TOKEN\";; esac\n").map_err(error)?;
                fs::set_permissions(&askpass, fs::Permissions::from_mode(0o700)).map_err(error)?;
                cmd.env("GIT_ASKPASS", askpass)
                    .env("GREENSHIFT_HISTORY_TOKEN", token.expose_secret());
            }
            #[cfg(not(unix))]
            {
                let _ = token;
                return Err(error(
                    "history token authentication is not supported on this platform",
                ));
            }
        }
        self.check_cancelled()?;
        let mut process = GitChild {
            child: cmd.spawn().map_err(error)?,
            complete: false,
        };
        let started = Instant::now();
        let status = loop {
            self.check_cancelled()?;
            if let Some(status) = process.child.try_wait().map_err(error)? {
                break status;
            }
            if started.elapsed() > self.timeout {
                return Err(error("git operation timed out"));
            }
            if stderr.metadata().map_err(error)?.len() > 1024 * 1024 {
                return Err(error("git diagnostics exceed size limit"));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if stderr.metadata().map_err(error)?.len() > 1024 * 1024 {
            return Err(error("git diagnostics exceed size limit"));
        }
        use std::io::{Seek, SeekFrom};
        let mut stdout = stdout;
        stdout.seek(SeekFrom::Start(0)).map_err(error)?;
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).map_err(error)?;
        if !status.success() {
            // Inspect only a narrow porcelain status. Never retain or display
            // raw diagnostics, because a remote can reflect credentials.
            let lease_rejected = std::str::from_utf8(&output).ok().is_some_and(|output| {
                output.lines().any(|line| {
                    line.starts_with("!\t")
                        && (line.ends_with("\t[rejected] (stale info)")
                            || line.ends_with("\t[remote rejected] (failed to update ref)")
                            || line.ends_with("\t[remote rejected] (incorrect old value provided)")
                            || line.ends_with("\t[remote rejected] (reference already exists)"))
                })
            });
            return Err(HistoryError::Git {
                status,
                lease_rejected,
            });
        }
        process.complete = true;
        Ok(output)
    }

    fn check_cancelled(&self) -> Result<(), HistoryError> {
        if self.cancellation.load(Ordering::SeqCst) {
            Err(HistoryError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Exact current ref identity, used as the stock cache action's key.
    pub fn head(&self) -> Result<Option<String>, HistoryError> {
        let bytes = self.git(["ls-remote", "--refs", &self.url, HISTORY_REF], None)?;
        let text = std::str::from_utf8(&bytes).map_err(error)?;
        let mut found = None;
        for line in text.lines() {
            let Some((oid, reference)) = line.split_once('\t') else {
                continue;
            };
            if reference != HISTORY_REF {
                continue;
            }
            if oid.len() == 64 {
                return Err(error("SHA-256 history remotes are not supported"));
            }
            if !valid_oid(oid) || found.is_some() {
                return Err(error("invalid remote history identity"));
            }
            found = Some(oid.to_owned());
        }
        Ok(found)
    }

    fn fetch(&self, oid: &str) -> Result<(), HistoryError> {
        self.git(["fetch", "--no-tags", "--depth=1", &self.url, oid], None)?;
        Ok(())
    }

    fn read(&self, oid: &str, path: &Path) -> Result<(), HistoryError> {
        self.fetch(oid)?;
        let tree = self.git(["ls-tree", oid], None)?;
        let tree = std::str::from_utf8(&tree).map_err(error)?;
        let parts: Vec<_> = tree.split_whitespace().collect();
        if parts.len() != 4
            || parts[0] != "100644"
            || parts[1] != "blob"
            || parts[3] != BLOB
            || !valid_oid(parts[2])
        {
            return Err(error(
                "history commit must contain exactly one regular snapshot",
            ));
        }
        let bytes = self.git(["cat-file", "blob", parts[2]], None)?;
        decompress(&bytes, path)?;
        snapshot::validate(path).map_err(error)
    }

    fn cached(&self, dir: &Path, oid: &str, stage: &Path) -> Result<(), HistoryError> {
        let directory = dir.join(oid);
        let commit = bounded_read(&directory.join("commit"), 16 * 1024)?;
        let identity = self.git(["hash-object", "-t", "commit", "--stdin"], Some(&commit))?;
        if identity.strip_suffix(b"\n") != Some(oid.as_bytes()) {
            return Err(error("cached commit identity mismatch"));
        }
        let commit = std::str::from_utf8(&commit).map_err(error)?;
        let tree = commit
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("tree "))
            .filter(|tree| valid_oid(tree))
            .ok_or_else(|| error("cached commit has no tree"))?;
        let mut bytes = Vec::new();
        cached_file(&directory.join(BLOB))?
            .read_to_end(&mut bytes)
            .map_err(error)?;
        let blob = self.git(["hash-object", "--stdin"], Some(&bytes))?;
        let blob = std::str::from_utf8(&blob).map_err(error)?.trim();
        let expected = self.git(
            ["mktree", "--missing"],
            Some(format!("100644 blob {blob}\t{BLOB}\n").as_bytes()),
        )?;
        if expected.strip_suffix(b"\n") != Some(tree.as_bytes()) {
            return Err(error("cached snapshot does not match canonical commit"));
        }
        decompress(&bytes, stage)?;
        snapshot::validate(stage).map_err(error)
    }

    /// Restore canonical history, optionally using the exact immutable cache
    /// copy. A missing ref does not treat an arbitrary cached DB as canonical.
    /// A committed restore includes a report: excluded incoming groups and
    /// retained local grouping limitations are not full recovery.
    pub fn restore(&self, db: &Path, cache: Option<&Path>) -> Result<RestoreOutcome, HistoryError> {
        check_restore_destination(db)?;
        let Some(oid) = self.head()? else {
            return Ok(RestoreOutcome::Missing);
        };
        let parent = db
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(error)?;
        let stage = tempfile::NamedTempFile::new_in(parent).map_err(error)?;
        let hit = cache.is_some_and(|dir| match self.cached(dir, &oid, stage.path()) {
            Ok(()) => true,
            Err(err) => {
                if dir.join(&oid).symlink_metadata().is_ok() {
                    eprintln!("greenshift: ⚠ cached history rejected: {err}");
                }
                false
            }
        });
        if !hit {
            self.read(&oid, stage.path())?;
        }
        self.check_cancelled()?;
        let report = self.persist_restore(stage, db)?;
        Ok(RestoreOutcome::Restored(report))
    }

    // Existing databases are updated through their SQLite inode. A missing
    // destination is created without clobbering a concurrent creator.
    fn persist_restore(
        &self,
        stage: tempfile::NamedTempFile,
        db: &Path,
    ) -> Result<MergeReport, HistoryError> {
        self.persist_restore_using(stage, db, create_restore, |path| fs::symlink_metadata(path))
    }

    fn persist_restore_using(
        &self,
        mut stage: tempfile::NamedTempFile,
        db: &Path,
        mut create: impl FnMut(tempfile::NamedTempFile, &Path) -> Result<File, tempfile::PersistError>,
        mut inspect: impl FnMut(&Path) -> std::io::Result<fs::Metadata>,
    ) -> Result<MergeReport, HistoryError> {
        loop {
            check_restore_destination(db)?;
            self.check_cancelled()?;
            match inspect(db) {
                Ok(_) => {
                    return snapshot::restore_existing(db, stage.path(), || {
                        self.cancellation.load(Ordering::SeqCst)
                    })
                    .map_err(error)
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => (),
                Err(err) => return Err(error(err)),
            }
            self.check_cancelled()?;
            match create(stage, db) {
                Ok(_) => return Ok(MergeReport::default()),
                Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    stage = err.file
                }
                Err(err) => return Err(error(err)),
            }
        }
    }

    /// Publish validated data, comparing the expected old identity. One rejected
    /// update refetches and merges; a second rejection is returned as a warning
    /// by the CLI. Neither concurrent saves nor caching serialize this operation.
    /// The outcome identifies whole incoming groups excluded by report overlap;
    /// committing the candidate does not promise those source groups are durable.
    pub fn publish(
        &self,
        artifact: &Path,
        cache: Option<&Path>,
    ) -> Result<PublishOutcome, HistoryError> {
        snapshot::validate(artifact).map_err(error)?;
        let current = self.scratch.path().join("current.sqlite");
        let candidate = self.scratch.path().join("candidate.sqlite");
        let mut old = self.head()?;
        let mut retried = false;
        loop {
            let report = if let Some(oid) = &old {
                self.read(oid, &current)?;
                snapshot::merge(&current, artifact, &candidate).map_err(error)?
            } else {
                snapshot::snapshot(artifact, &candidate).map_err(error)?;
                MergeReport::default()
            };
            let compressed = compress(&candidate)?;
            let blob = self.git(["hash-object", "-w", "--stdin"], Some(&compressed))?;
            let blob = std::str::from_utf8(&blob).map_err(error)?.trim();
            let tree = self.git(
                ["mktree"],
                Some(format!("100644 blob {blob}\t{BLOB}\n").as_bytes()),
            )?;
            let tree = std::str::from_utf8(&tree).map_err(error)?.trim();
            let commit = self.git(["commit-tree", tree], Some(b"greenshift history\n"))?;
            let oid = std::str::from_utf8(&commit)
                .map_err(error)?
                .trim()
                .to_owned();
            let lease = format!(
                "--force-with-lease={HISTORY_REF}:{}",
                old.as_deref().unwrap_or("")
            );
            let refspec = format!("{oid}:{HISTORY_REF}");
            match self.git(["push", "--porcelain", &lease, &self.url, &refspec], None) {
                Ok(_) => {
                    if let Some(dir) = cache {
                        let cached = (|| -> Result<(), HistoryError> {
                            let path = dir.join(&oid);
                            fs::create_dir_all(&path).map_err(error)?;
                            fs::write(path.join(BLOB), compressed).map_err(error)?;
                            let commit = self.git(["cat-file", "commit", &oid], None)?;
                            fs::write(path.join("commit"), commit).map_err(error)?;
                            Ok(())
                        })();
                        if let Err(err) = cached {
                            eprintln!(
                                "greenshift: ⚠ history published, but cache copy failed: {err}"
                            );
                        }
                    }
                    return Ok(PublishOutcome::new(oid, report));
                }
                Err(
                    err @ HistoryError::Git {
                        lease_rejected: true,
                        ..
                    },
                ) if !retried => {
                    let head = self.head()?;
                    if head == old {
                        return Err(err);
                    }
                    retried = true;
                    old = head;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

fn create_restore(
    stage: tempfile::NamedTempFile,
    db: &Path,
) -> Result<File, tempfile::PersistError> {
    stage.persist_noclobber(db)
}

fn valid_oid(oid: &str) -> bool {
    oid.len() == 40 && oid.bytes().all(|b| b.is_ascii_hexdigit())
}

fn cached_file(path: &Path) -> Result<File, HistoryError> {
    // Reject symlinks and special files before opening, then verify the opened
    // handle too. O_NONBLOCK closes the FIFO race between those two checks.
    if !fs::symlink_metadata(path).map_err(error)?.is_file() {
        return Err(error("cached history must be a regular file"));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(error)?;
    if !file.metadata().map_err(error)?.is_file() {
        return Err(error("cached history must be a regular file"));
    }
    Ok(file)
}

fn bounded_read(path: &Path, cap: u64) -> Result<Vec<u8>, HistoryError> {
    // Only commit metadata is bounded; the actual history blob is not.
    let file = cached_file(path)?;
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes).map_err(error)?;
    if bytes.len() as u64 > cap {
        return Err(error("cached commit exceeds metadata size limit"));
    }
    Ok(bytes)
}

fn check_restore_destination(path: &Path) -> Result<(), HistoryError> {
    snapshot::check_sidecars(path).map_err(error)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            crate::store::Store::open_read_only(path).map_err(error)?;
            Ok(())
        }
        Ok(_) => Err(error(
            "history destination must be a regular greenshift database",
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(error(err)),
    }
}

fn compress(path: &Path) -> Result<Vec<u8>, HistoryError> {
    let mut reader = File::open(path).map_err(error)?;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::copy(&mut reader, &mut encoder).map_err(error)?;
    encoder.finish().map_err(error)
}

fn decompress(bytes: &[u8], path: &Path) -> Result<(), HistoryError> {
    let mut decoder = flate2::read::GzDecoder::new(bytes);
    let mut output = File::create(path).map_err(error)?;
    std::io::copy(&mut decoder, &mut output).map_err(error)?;
    output.sync_all().map_err(error)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn fixture(path: &Path, source: &str) {
        crate::store::Store::open(path)
            .unwrap()
            .record_run(
                &crate::store::NewRun {
                    commit: None,
                    source,
                    files: &[],
                    partial: false,
                    branch: None,
                    tag: None,
                },
                std::iter::empty(),
            )
            .unwrap();
    }

    fn remote_repository(path: &Path) -> Remote {
        assert!(Command::new("git")
            .args(["init", "--bare"])
            .arg(path)
            .output()
            .unwrap()
            .status
            .success());
        Remote::new(path.to_str().unwrap()).unwrap()
    }

    #[test]
    fn test_persist_restore_cancelled_after_validation_preserves_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("existing.sqlite");
        fixture(&destination, "unpublished-local");
        let before = fs::read(&destination).unwrap();
        let stage = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        fixture(stage.path(), "canonical-remote");
        snapshot::validate(stage.path()).unwrap();
        let cancellation = Arc::new(AtomicBool::new(false));
        let remote = Remote {
            scratch: tempfile::tempdir().unwrap(),
            url: String::new(),
            token: None,
            timeout: TIMEOUT,
            cancellation: Arc::clone(&cancellation),
        };
        // No Git runs here: the interrupt arrives after local validation and
        // must still fence the actual atomic replacement.
        cancellation.store(true, Ordering::SeqCst);
        assert!(remote
            .persist_restore(stage, &destination)
            .unwrap_err()
            .is_cancelled());
        assert_eq!(fs::read(destination).unwrap(), before);
    }

    #[test]
    fn test_persist_restore_precancelled_missing_destination_skips_branch_inspection() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("absent.sqlite");
        let stage = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        fixture(stage.path(), "canonical-remote");
        let remote = Remote {
            scratch: tempfile::tempdir().unwrap(),
            url: String::new(),
            token: None,
            timeout: TIMEOUT,
            cancellation: Arc::new(AtomicBool::new(true)),
        };
        // This covers the early check before choosing create or update, not
        // the final create fence after that branch's destination observation.
        let mut branch_inspected = false;
        let result = remote.persist_restore_using(stage, &destination, create_restore, |path| {
            branch_inspected = true;
            fs::symlink_metadata(path)
        });
        assert!(result.unwrap_err().is_cancelled());
        assert!(!branch_inspected);
        assert!(!destination.exists());
    }

    #[test]
    fn test_persist_restore_cancelled_after_missing_path_check_never_creates() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("absent.sqlite");
        let stage = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        fixture(stage.path(), "canonical-remote");
        let cancellation = Arc::new(AtomicBool::new(false));
        let remote = Remote {
            scratch: tempfile::tempdir().unwrap(),
            url: String::new(),
            token: None,
            timeout: TIMEOUT,
            cancellation: Arc::clone(&cancellation),
        };
        let mut inspected = false;
        let mut created = false;
        let result = remote.persist_restore_using(
            stage,
            &destination,
            |stage, path| {
                created = true;
                create_restore(stage, path)
            },
            |path| {
                let observed = fs::symlink_metadata(path);
                assert_eq!(
                    observed.as_ref().unwrap_err().kind(),
                    std::io::ErrorKind::NotFound
                );
                inspected = true;
                // An interrupt after the actual path observation must still
                // prevent the subsequent creation side effect.
                cancellation.store(true, Ordering::SeqCst);
                observed
            },
        );
        assert!(inspected);
        assert!(result.unwrap_err().is_cancelled());
        assert!(!created);
        assert!(!destination.exists());
    }

    #[test]
    fn test_create_restore_collision_retains_stage_then_unions_new_local_database() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("created.sqlite");
        let stage = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        fixture(stage.path(), "remote");
        assert!(!destination.exists());
        let remote = Remote {
            scratch: tempfile::tempdir().unwrap(),
            url: String::new(),
            token: None,
            timeout: TIMEOUT,
            cancellation: Arc::new(AtomicBool::new(false)),
        };
        let mut attempts = 0;
        remote
            .persist_restore_using(
                stage,
                &destination,
                |stage, db| {
                    attempts += 1;
                    // An ordinary creator wins after the real missing-path check. The
                    // creation primitive and its caller must retain our stage, then
                    // retry via the existing database transaction instead of replacing.
                    assert!(!db.exists());
                    fixture(db, "new-local");
                    let before = fs::read(db).unwrap();
                    let collision = create_restore(stage, db).unwrap_err();
                    assert_eq!(collision.error.kind(), std::io::ErrorKind::AlreadyExists);
                    assert_eq!(fs::read(db).unwrap(), before);
                    snapshot::validate(collision.file.path()).unwrap();
                    Err(collision)
                },
                |path| fs::symlink_metadata(path),
            )
            .unwrap();
        assert_eq!(attempts, 1);
        let conn = rusqlite::Connection::open(&destination).unwrap();
        let sources = conn
            .prepare("SELECT source FROM runs ORDER BY source")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(sources, vec!["new-local", "remote"]);
    }

    #[test]
    fn test_snapshot_transaction_cancellation_keeps_history_cancellation_semantics() {
        assert!(HistoryError::from(snapshot::SnapshotError::Cancelled).is_cancelled());
    }

    #[test]
    fn test_decompress_preserves_payload_and_rejects_corrupt_gzip() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        let output = dir.path().join("output");
        fs::write(&input, vec![b'x'; 1000]).unwrap();
        let gzip = compress(&input).unwrap();
        decompress(&gzip, &output).unwrap();
        assert_eq!(fs::read(&output).unwrap(), vec![b'x'; 1000]);
        assert!(matches!(
            decompress(b"not a gzip", &output),
            Err(HistoryError::Io(_))
        ));
    }

    #[test]
    fn test_cached_forged_commit_over_matching_substitute_tree_rejected_by_identity() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().join("canonical");
        let substitute = dir.path().join("substitute");
        fixture(&canonical, "canonical");
        fixture(&substitute, "substitute");
        let remote = remote_repository(&dir.path().join("remote.git"));
        let other = remote_repository(&dir.path().join("other.git"));
        let cache = dir.path().join("cache");
        let oid = remote
            .publish(&canonical, Some(&cache))
            .unwrap()
            .oid()
            .to_owned();
        let other_cache = dir.path().join("other-cache");
        let other_oid = other
            .publish(&substitute, Some(&other_cache))
            .unwrap()
            .oid()
            .to_owned();
        for name in ["commit", BLOB] {
            fs::copy(
                other_cache.join(&other_oid).join(name),
                cache.join(&oid).join(name),
            )
            .unwrap();
        }
        assert!(matches!(
            remote.cached(&cache, &oid, &dir.path().join("stage")),
            Err(HistoryError::Invalid("cached commit identity mismatch"))
        ));
        let output = dir.path().join("read");
        remote.restore(&output, Some(&cache)).unwrap();
        let source: String = rusqlite::Connection::open(output)
            .unwrap()
            .query_row("SELECT source FROM runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(source, "canonical");
    }

    #[test]
    fn test_read_wrong_name_mode_and_extra_tree_entries_rejected_as_shape() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("db");
        fixture(&database, "canonical");
        let remote = remote_repository(&dir.path().join("remote.git"));
        let gzip = compress(&database).unwrap();
        let blob = remote
            .git(["hash-object", "-w", "--stdin"], Some(&gzip))
            .unwrap();
        let blob = std::str::from_utf8(&blob).unwrap().trim();
        for description in [
            format!("100755 blob {blob}\t{BLOB}\n"),
            format!("100644 blob {blob}\twrong-name\n"),
            format!("100644 blob {blob}\t{BLOB}\n100644 blob {blob}\tsecond\n"),
        ] {
            let tree = remote
                .git(["mktree"], Some(description.as_bytes()))
                .unwrap();
            let tree = std::str::from_utf8(&tree).unwrap().trim();
            let commit = remote
                .git(["commit-tree", tree], Some(b"malformed history\n"))
                .unwrap();
            let commit = std::str::from_utf8(&commit).unwrap().trim();
            remote
                .git(
                    [
                        "push",
                        "--force",
                        &remote.url,
                        &format!("{commit}:{HISTORY_REF}"),
                    ],
                    None,
                )
                .unwrap();
            assert!(matches!(
                remote.restore(&dir.path().join("read"), None),
                Err(HistoryError::Invalid(
                    "history commit must contain exactly one regular snapshot"
                ))
            ));
        }
    }

    #[test]
    fn test_git_scrub_keeps_ca_trust_and_removes_the_rest() {
        let keys = [
            "GIT_SSL_CAINFO",
            "GIT_SSL_CAPATH",
            "GIT_SSL_NO_VERIFY",
            "GIT_DIR",
            "GIT_CONFIG_GLOBAL",
            "GIT_SSH_COMMAND",
            "GIT_ASKPASS",
            "SSL_CERT_FILE",
            "PATH",
        ]
        .map(std::ffi::OsString::from);
        let removed = scrubbed_git_vars(keys);
        assert_eq!(
            removed,
            [
                "GIT_SSL_NO_VERIFY",
                "GIT_DIR",
                "GIT_CONFIG_GLOBAL",
                "GIT_SSH_COMMAND",
                "GIT_ASKPASS",
            ]
            .map(std::ffi::OsString::from)
        );
    }

    #[test]
    fn test_git_scrub_ignores_case_where_the_environment_does() {
        let keys = [
            "Git_Dir",
            "git_askpass",
            "Git_Ssl_CaInfo",
            "GIT_SSL_CAPATH",
            "Path",
        ]
        .map(std::ffi::OsString::from);
        assert_eq!(
            scrubbed_git_vars_with_case(keys.clone(), true),
            ["Git_Dir", "git_askpass"].map(std::ffi::OsString::from),
            "on Windows a mixed-case GIT_ variable is removed and mixed-case CA trust is kept"
        );
        assert!(
            scrubbed_git_vars_with_case(keys, false).is_empty(),
            "where names are case-sensitive, git reads only the upper-case names"
        );
        assert_eq!(
            scrubbed_git_vars(["Git_Dir"].map(std::ffi::OsString::from)).is_empty(),
            !cfg!(windows),
            "the scrub picks its case mode from the platform"
        );
    }

    #[test]
    fn test_git_stalled_stdin_reader_is_still_bounded_by_operation_timeout() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut remote = remote_repository(&dir.path().join("remote.git"));
        remote.timeout = Duration::from_millis(150);
        let script = dir.path().join("stalled-reader");
        fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let started = Instant::now();
        let error = remote
            .git_program(
                script.as_os_str(),
                ["hash-object", "--stdin"],
                Some(&vec![0; 1024 * 1024]),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            HistoryError::Invalid("git operation timed out")
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn test_cancellation_before_constructor_or_during_child_is_bounded() {
        use std::os::unix::fs::PermissionsExt;
        let cancellation = Arc::new(AtomicBool::new(true));
        assert!(matches!(
            Remote::new_cancellable("/does/not/exist", cancellation.clone()),
            Err(HistoryError::Cancelled)
        ));
        cancellation.store(false, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("remote.git");
        assert!(Command::new("git")
            .args(["init", "--bare"])
            .arg(&repo)
            .output()
            .unwrap()
            .status
            .success());
        let remote = Remote::new_cancellable(repo.to_str().unwrap(), cancellation.clone()).unwrap();
        let script = dir.path().join("waiting");
        fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                cancellation.store(true, Ordering::SeqCst);
            });
            let started = Instant::now();
            let result = remote.git_program(
                script.as_os_str(),
                ["hash-object", "--stdin"],
                Some(&vec![0; 1024 * 1024]),
            );
            assert!(result.unwrap_err().is_cancelled());
            assert!(started.elapsed() < Duration::from_secs(2));
        });
        assert!(remote.head().unwrap_err().is_cancelled());
    }

    #[test]
    fn test_restore_legacy_v3_ref_then_publish_upgrades_without_losing_rows() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("legacy");
        fixture(&database, "legacy");
        rusqlite::Connection::open(&database).unwrap().execute_batch("DROP TABLE report_memberships; DROP INDEX runs_by_history_id; ALTER TABLE runs DROP COLUMN history_id; PRAGMA user_version=3;").unwrap();
        let remote = remote_repository(&dir.path().join("remote.git"));
        let blob = remote
            .git(
                ["hash-object", "-w", "--stdin"],
                Some(&compress(&database).unwrap()),
            )
            .unwrap();
        let blob = std::str::from_utf8(&blob).unwrap().trim();
        let tree = remote
            .git(
                ["mktree"],
                Some(format!("100644 blob {blob}\t{BLOB}\n").as_bytes()),
            )
            .unwrap();
        let commit = remote
            .git(
                ["commit-tree", std::str::from_utf8(&tree).unwrap().trim()],
                Some(b"legacy snapshot\n"),
            )
            .unwrap();
        let oid = std::str::from_utf8(&commit).unwrap().trim();
        remote
            .git(["push", &remote.url, &format!("{oid}:{HISTORY_REF}")], None)
            .unwrap();
        let output = dir.path().join("read");
        remote.restore(&output, None).unwrap();
        let conn = rusqlite::Connection::open(&output).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM runs", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(conn);
        remote.publish(&output, None).unwrap();
        let fresh = dir.path().join("fresh");
        remote.restore(&fresh, None).unwrap();
        let conn = rusqlite::Connection::open(fresh).unwrap();
        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            crate::store::SCHEMA_VERSION
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM runs", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn test_publish_keyed_import_transport_restore_retains_replay_identity() {
        use crate::report_key::{ReportKey, ReportKeySet};
        use crate::store::{ImportAcknowledgement, NewRun, Store};
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("source");
        let xml = r#"<testsuite name="gears"><testcase name="turns"/></testsuite>"#;
        let report = crate::junit::parse_str(xml).unwrap();
        let keys = ReportKeySet::new([ReportKey::from_bytes(None, xml.as_bytes())]).unwrap();
        let run = NewRun {
            commit: None,
            source: "transport-fixture",
            files: &[],
            partial: false,
            branch: None,
            tag: None,
        };
        let mut store = Store::open(&database).unwrap();
        store.record_import(&run, &keys, report.cases()).unwrap();
        drop(store);
        let remote = remote_repository(&dir.path().join("remote.git"));
        remote.publish(&database, None).unwrap();
        let restored = dir.path().join("restored");
        remote.restore(&restored, None).unwrap();
        let mut store = Store::open(&restored).unwrap();
        assert!(matches!(
            store.record_import(&run, &keys, report.cases()).unwrap(),
            ImportAcknowledgement::AlreadyPresent { .. }
        ));
        drop(store);
        remote.publish(&restored, None).unwrap();
        let fresh = dir.path().join("fresh");
        remote.restore(&fresh, None).unwrap();
        let conn = rusqlite::Connection::open(&fresh).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM runs", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM report_memberships", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn test_git_timeout_terminates_transport_helper_group() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/history.git", listener.local_addr().unwrap());
        let mut remote = Remote::new(&url).unwrap();
        remote.timeout = Duration::from_millis(150);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let deadline = Instant::now() + Duration::from_secs(1);
                let stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "git never connected to timeout fixture"
                            );
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(err) => panic!("timeout fixture accept failed: {err}"),
                    }
                };
                // A stalled peer outlives the operation timeout.
                std::thread::sleep(Duration::from_millis(400));
                drop(stream);
            });
            // The test's own peer must not be routed through inherited proxies.
            let result = remote.git(
                [
                    "-c",
                    "http.proxy=",
                    "ls-remote",
                    "--refs",
                    &url,
                    HISTORY_REF,
                ],
                None,
            );
            assert!(result.unwrap_err().to_string().contains("timed out"));
            // Directly inspect the OS reader, not merely git's parent status.
            // A killed orphan may remain briefly as an empty-cmdline zombie.
            let deadline = Instant::now() + Duration::from_millis(150);
            loop {
                let alive = fs::read_dir("/proc")
                    .unwrap()
                    .filter_map(Result::ok)
                    .any(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .bytes()
                            .all(|b| b.is_ascii_digit())
                            && fs::read(entry.path().join("cmdline"))
                                .ok()
                                .is_some_and(|bytes| {
                                    bytes.windows(url.len()).any(|part| part == url.as_bytes())
                                })
                    });
                if !alive {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "git transport helper survived timeout"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        });
    }
}
