//! Backup / restore of the whole Clawket store.
//!
//! ## Archive format — why a gzipped SQLite file, not a tar
//!
//! The CLI help originally promised a "tar.gz archive (DB + attached knowledge
//! entries)". That framing is a leftover from an era where knowledge lived on
//! disk: since `migrations/024_artifacts_to_knowledge.sql` every knowledge entry
//! is a row in `knowledge` inside the same `db.sqlite`. There is exactly one
//! file to carry, so a tar would be a container around a single member — a new
//! dependency (`tar` is not in `Cargo.toml`) buying nothing. The CLI help text
//! was corrected to describe the real format instead.
//!
//! What we write is therefore:
//!
//! ```text
//!   gzip( CLWKBAK1 | u32le header_len | header_json | sqlite_bytes )
//! ```
//!
//! The magic + JSON header make the archive self-describing: `restore` can
//! validate the schema version, and the daemon build that produced it, *before*
//! it touches the live database. A bare gzipped SQLite file could not carry
//! that, and sniffing the schema would mean opening an untrusted file as a
//! database first.
//!
//! The snapshot itself comes from SQLite's `VACUUM INTO`, which serialises a
//! transactionally consistent copy of the database (readers see a single
//! snapshot; no torn WAL state) and drops free pages along the way. That is the
//! same guarantee the online backup API gives, with one statement.
//!
//! ## Restore atomicity
//!
//! `restore` never writes into the live database file. It decodes to a temp
//! file *next to* the destination (same filesystem, so the final step can be a
//! rename), opens it to verify it is a real SQLite database with a sane
//! `schema_version`, and only then swaps it in. A failure at any earlier point
//! leaves the original untouched. The pre-swap copy of the original is kept as
//! `db.sqlite.pre-restore` so a bad-but-valid archive is still recoverable.
//!
//! Note the daemon holds an open pool against the replaced file: the swap is
//! visible to new connections, and callers are told to restart. Doing the swap
//! under the process would require quiescing the pool, which the local
//! single-user contract does not justify.

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::db::SCHEMA_VERSION_MAX;

/// Archive magic. Bumping the trailing digit is a format break; `restore`
/// rejects anything it does not recognise rather than guessing.
const MAGIC: &[u8; 8] = b"CLWKBAK1";

/// Upper bound on the JSON header. Guards against a corrupt/hostile length
/// prefix making us allocate wildly before we have read anything.
const MAX_HEADER_LEN: u32 = 64 * 1024;

/// Metadata stored in the archive header.
#[derive(Debug, Serialize, Deserialize)]
pub struct BackupHeader {
    /// Schema version of the database at the time of the backup.
    pub schema_version: i64,
    /// Milliseconds since epoch when the backup was taken.
    pub created_at: i64,
    /// Daemon version that produced the archive (informational).
    pub daemon_version: String,
    /// Project this backup was scoped to, if any. `None` = whole store.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

/// Result of a successful `backup`.
#[derive(Debug, Serialize)]
pub struct BackupResult {
    pub output: String,
    pub bytes: u64,
    pub schema_version: i64,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

/// Result of a `restore` (both dry-run and applied).
#[derive(Debug, Serialize)]
pub struct RestoreResult {
    pub input: String,
    pub dry_run: bool,
    /// True when the live database file was actually replaced.
    pub restored: bool,
    /// Schema version recorded in the archive.
    pub archive_schema_version: i64,
    /// Schema version of the database being replaced (0 if there was none).
    pub current_schema_version: i64,
    pub archive_created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_project_id: Option<String>,
    /// Where the pre-restore copy of the live database was kept, when a swap
    /// happened. Absent on dry runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_db_backup: Option<String>,
    /// Human-readable summary of what happened / would happen.
    pub message: String,
}

/// Reject paths that are obviously not a place a user meant to name.
///
/// Threat model: the daemon is local and single-user (`CLAUDE.md`) — it runs as
/// the user, so it can already read and write anything the user can, and the
/// caller *is* the user. There is no privilege boundary to defend here, and
/// confining backups to a fixed directory would break the stated purpose
/// ("cross-machine transfer or offsite backup" needs arbitrary destinations).
///
/// What is still worth blocking is the mistake case: a relative path resolved
/// against the daemon's cwd (which the user cannot see and which is not where
/// they think they are), and NUL bytes that would be truncated by the OS into a
/// different path than the one that was validated. So we require an absolute,
/// lexically normalised path and let everything else through deliberately.
fn sanitize_path(raw: &str, field: &str) -> Result<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("INVALID_BACKUP_PATH: `{field}` must not be empty");
    }
    if trimmed.contains('\0') {
        bail!("INVALID_BACKUP_PATH: `{field}` must not contain NUL bytes");
    }
    let p = Path::new(trimmed);
    if !p.is_absolute() {
        bail!(
            "INVALID_BACKUP_PATH: `{field}` must be an absolute path (got `{trimmed}`); \
             a relative path would resolve against the daemon's working directory, not yours"
        );
    }
    // Lexical normalisation: collapse `.` and `..` so the path we report is the
    // path we act on. We do not canonicalise — the output file does not exist
    // yet, and canonicalize() fails on non-existent paths.
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    bail!("INVALID_BACKUP_PATH: `{field}` escapes the filesystem root");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

/// Remove restore staging files left by a daemon that died mid-restore.
///
/// The in-flight error paths clean up after themselves, but a kill -9 between
/// staging and rename leaves `.clawket-restore-<pid>.<seq>.tmp` in the data
/// directory forever — one full database copy each. Called once at startup,
/// when by definition no restore of ours is in flight. Best-effort: a sweep
/// that fails must never stop the daemon coming up.
pub fn sweep_restore_temps(db_path: &Path) {
    let Some(dir) = db_path.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".clawket-restore-") && name.ends_with(".tmp") {
            match std::fs::remove_file(e.path()) {
                Ok(()) => tracing::info!(file = %name, "removed stale restore staging file"),
                Err(err) => {
                    tracing::debug!(file = %name, "could not remove stale staging file: {err}")
                }
            }
        }
    }
}

/// Monotonic per-process counter. Temp paths derived from the pid alone collide
/// between concurrent requests inside one daemon — every `/backup` and
/// `/restore` shares a process.
fn next_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// A temp path beside `target`, distinct for every caller.
///
/// Appends rather than replacing the extension: `with_extension` would map
/// `snap.gz` and `snap.tar` to the same temp name. Sibling (not `/tmp`) because
/// the final step is a rename, which is only atomic within one filesystem.
fn sibling_temp(target: &Path, tag: &str) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "clawket".to_string());
    name.push_str(&format!(".{tag}.tmp"));
    match target.parent() {
        Some(dir) => dir.join(name),
        None => PathBuf::from(name),
    }
}

/// Default archive name when the caller did not pass `--output`.
pub fn default_output_path(now_ms: i64) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    cwd.join(format!("clawket-backup-{now_ms}.clawketbak.gz"))
}

/// Write a compressed, self-describing snapshot of `db_path` to `output`.
pub fn backup(
    db_path: &Path,
    output: &Path,
    project_id: Option<&str>,
    schema_version: i64,
    now_ms: i64,
) -> Result<BackupResult> {
    // Refuse to write onto the store we are copying, or onto its WAL/journal
    // sidecars. `File::create` truncates unconditionally, so `--output <the db>`
    // replaces the database with gzip magic and then reports success — the user
    // is told the backup worked at the moment their data is gone, and unlike
    // restore there is no pre-swap copy to fall back on.
    //
    // Compared by file IDENTITY, not by string. An earlier version compared the
    // two paths lexically on the claim that both were normalised; only `output`
    // is (`sanitize_path`). `db_path` arrives verbatim from `Paths::resolve()`,
    // so `CLAWKET_DB`, `--db`, `CLAWKET_DATA_DIR` and `XDG_DATA_HOME` all pass
    // their spelling straight through. Any difference defeated a string compare
    // — `/tmp` vs its `/private/tmp` symlink target on macOS is enough, and
    // needs no unusual input at all. Measured: the store went from a 569 KB
    // SQLite file to a 12 KB gzip, with HTTP 200.
    //
    // The destination usually does not exist yet, so canonicalise its DIRECTORY
    // (created just above) and re-join the file name. That resolves symlinks and
    // `..` on both sides while still naming a file that is not there.
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create backup dir: {}", parent.display()))?;
    }
    let resolved_output = match (output.parent(), output.file_name()) {
        (Some(dir), Some(name)) => dir.canonicalize().map(|d| d.join(name)).ok(),
        _ => None,
    };
    let resolved_db = db_path.canonicalize().ok();
    let collides = match (&resolved_output, &resolved_db) {
        (Some(o), Some(d)) => {
            let ds = d.to_string_lossy();
            let os = o.to_string_lossy();
            os == ds
                || os == format!("{ds}-wal")
                || os == format!("{ds}-shm")
                || os == format!("{ds}-journal")
        }
        // Either side unresolvable (db missing on a first run, output parent
        // gone). Fall back to the lexical comparison: weaker, but it still
        // catches the exact-spelling case, and refusing every backup because a
        // path could not be canonicalised would be worse.
        _ => {
            let ds = db_path.to_string_lossy();
            let os = output.to_string_lossy();
            os == ds
                || os == format!("{ds}-wal")
                || os == format!("{ds}-shm")
                || os == format!("{ds}-journal")
        }
    };
    if collides {
        bail!(
            "INVALID_BACKUP_PATH: `output` ({}) is the live database or one of its \
             sidecar files. Writing there would destroy the store this backup is of.",
            output.display()
        );
    }

    // LM-8: `~/.claude/plugins/` is deleted wholesale by `/plugin install`. A
    // backup written there is destroyed by the next reinstall — precisely when
    // the user is most likely to want it. `sanitize_path` deliberately allows
    // arbitrary destinations (offsite backup needs that), so this is the one
    // location worth carving out, and the invariant already has a helper.
    if crate::paths::path_overlaps_plugin_dir(output) {
        bail!(
            "INVALID_BACKUP_PATH: `output` ({}) is under the Claude Code plugin directory, \
             which is deleted on every plugin reinstall. Choose a location outside it.",
            output.display()
        );
    }

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create backup dir: {}", parent.display()))?;
    }

    // Both temp paths carry a per-call nonce, not just the pid.
    //
    // The snapshot path was `output.with_extension(...)`, which REPLACES the
    // extension: `snap.gz` and `snap.tar` both mapped to `snap.sqlite.snapshot.tmp`,
    // and the unconditional remove_file below then let two concurrent backups
    // delete each other's snapshot mid-read. Measured over 30 concurrent
    // iterations: 30 hard errors and 2 archives that returned Ok but could not
    // be decoded — a backup that reports success and cannot be restored is the
    // worst outcome this feature has.
    let nonce = format!("{}.{}", std::process::id(), next_seq());
    let snapshot_path = sibling_temp(output, &format!("snapshot.{nonce}"));
    let _ = std::fs::remove_file(&snapshot_path);

    // Scope the connection so the snapshot file is closed before we read it.
    {
        let conn = Connection::open(db_path)
            .with_context(|| format!("open sqlite for backup: {}", db_path.display()))?;
        // `VACUUM INTO` runs inside an implicit read transaction, so the copy is
        // a consistent snapshot even while other pool connections keep writing.
        conn.execute("VACUUM INTO ?1", [snapshot_path.to_string_lossy().as_ref()])
            .with_context(|| format!("VACUUM INTO {}", snapshot_path.display()))?;
    }

    let write_result = (|| -> Result<u64> {
        let sqlite_bytes = std::fs::read(&snapshot_path)
            .with_context(|| format!("read snapshot: {}", snapshot_path.display()))?;

        let header = BackupHeader {
            schema_version,
            created_at: now_ms,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            project_id: project_id.map(str::to_string),
        };
        let header_json = serde_json::to_vec(&header).context("serialize backup header")?;
        if header_json.len() as u64 > MAX_HEADER_LEN as u64 {
            bail!("BACKUP_FAILED: header exceeds {MAX_HEADER_LEN} bytes");
        }

        // Write to a private temp file and rename it into place, the same shape
        // restore already uses. Three problems collapse into that one change:
        // a crash mid-write can no longer leave a truncated archive at the name
        // the user will later restore from; two concurrent backups to the same
        // output cannot interleave their bytes; and `O_EXCL` on the temp means
        // we never follow a symlink at the destination during the write.
        //
        // The rename still replaces an existing file at `output` — that is what
        // a caller re-running `--output backup.gz` asks for, and refusing would
        // make routine re-backup fail. What it no longer does is destroy the
        // old archive before the new one is known-good.
        let staged = sibling_temp(output, &format!("write.{nonce}"));
        let _ = std::fs::remove_file(&staged);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
            .with_context(|| format!("create staged backup: {}", staged.display()))?;
        let mut enc = GzEncoder::new(file, Compression::default());
        enc.write_all(MAGIC)?;
        enc.write_all(&(header_json.len() as u32).to_le_bytes())?;
        enc.write_all(&header_json)?;
        enc.write_all(&sqlite_bytes)?;
        let mut done = enc.finish().context("finish gzip stream")?;
        // fsync before publishing: the rename only orders the directory entry,
        // not the content that entry points at.
        done.flush().context("flush staged backup")?;
        done.sync_all().context("fsync staged backup")?;
        drop(done);

        if let Err(e) = std::fs::rename(&staged, output) {
            let _ = std::fs::remove_file(&staged);
            return Err(e).with_context(|| format!("publish backup to {}", output.display()));
        }

        let bytes = std::fs::metadata(output)
            .with_context(|| format!("stat backup file: {}", output.display()))?
            .len();
        Ok(bytes)
    })();

    // Always clean up the intermediate snapshot, success or failure.
    let _ = std::fs::remove_file(&snapshot_path);
    let bytes = write_result?;

    Ok(BackupResult {
        output: output.to_string_lossy().into_owned(),
        bytes,
        schema_version,
        created_at: now_ms,
        project_id: project_id.map(str::to_string),
    })
}

/// Decode an archive into `(header, sqlite_bytes)`.
fn decode_archive(input: &Path) -> Result<(BackupHeader, Vec<u8>)> {
    let file = std::fs::File::open(input)
        .with_context(|| format!("open backup archive: {}", input.display()))?;
    let mut dec = GzDecoder::new(file);

    let mut magic = [0u8; 8];
    dec.read_exact(&mut magic)
        .map_err(|e| anyhow::anyhow!("BACKUP_ARCHIVE_INVALID: not a readable gzip archive: {e}"))?;
    if &magic != MAGIC {
        bail!(
            "BACKUP_ARCHIVE_INVALID: bad magic — `{}` is not a Clawket backup archive",
            input.display()
        );
    }

    let mut len_buf = [0u8; 4];
    dec.read_exact(&mut len_buf)
        .map_err(|e| anyhow::anyhow!("BACKUP_ARCHIVE_INVALID: truncated header length: {e}"))?;
    let header_len = u32::from_le_bytes(len_buf);
    if header_len == 0 || header_len > MAX_HEADER_LEN {
        bail!("BACKUP_ARCHIVE_INVALID: implausible header length {header_len}");
    }

    let mut header_buf = vec![0u8; header_len as usize];
    dec.read_exact(&mut header_buf)
        .map_err(|e| anyhow::anyhow!("BACKUP_ARCHIVE_INVALID: truncated header: {e}"))?;
    let header: BackupHeader = serde_json::from_slice(&header_buf)
        .map_err(|e| anyhow::anyhow!("BACKUP_ARCHIVE_INVALID: unreadable header: {e}"))?;

    let mut sqlite_bytes = Vec::new();
    dec.read_to_end(&mut sqlite_bytes)
        .map_err(|e| anyhow::anyhow!("BACKUP_ARCHIVE_INVALID: truncated payload: {e}"))?;
    if sqlite_bytes.is_empty() {
        bail!("BACKUP_ARCHIVE_INVALID: archive contains no database payload");
    }

    Ok((header, sqlite_bytes))
}

/// Read the schema version of an on-disk SQLite file without running migrations.
fn read_schema_version(path: &Path) -> Result<i64> {
    // Same reasoning as the integrity failure below: a payload SQLite refuses to
    // open is a bad archive (the caller's input), not a daemon fault.
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| {
            anyhow::anyhow!("BACKUP_ARCHIVE_INVALID: payload is not a readable database: {e}")
        })?;
    // `integrity_check` on a freshly decoded file is the cheapest way to reject
    // a payload that is gzip-valid but not a usable database.
    //
    // Damage severe enough breaks the PRAGMA itself rather than returning a
    // verdict, so that failure carries the same code: either way the caller
    // handed us an unusable archive, which is a 400, not a daemon fault. Without
    // this the query error escaped uncoded and surfaced as a 500.
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| {
            anyhow::anyhow!("BACKUP_ARCHIVE_INVALID: payload is not a readable database: {e}")
        })?;
    if integrity != "ok" {
        bail!("BACKUP_ARCHIVE_INVALID: payload failed integrity_check: {integrity}");
    }
    let has_table: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !has_table {
        bail!("BACKUP_ARCHIVE_INVALID: payload has no schema_version table");
    }
    let v: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .context("read schema_version")?;
    Ok(v)
}

/// Restore `input` over `db_path`.
///
/// `merge` is accepted only to be refused — see the error message for why.
pub fn restore(
    db_path: &Path,
    input: &Path,
    merge: bool,
    dry_run: bool,
    current_schema_version: i64,
) -> Result<RestoreResult> {
    // Refuse merge before doing any work. Merging two Clawket stores means
    // remapping opaque generated ids (`new_id`) across every FK edge, then
    // reconciling uniqueness invariants that are not expressible as row
    // conflicts: one active plan per project, one active cycle per unit, unique
    // `projects.cwd` (migration 019). There is no correct automatic answer for
    // those, and picking one silently would corrupt the workflow state the
    // daemon exists to guarantee. Refusing is honest; replacing behind the
    // user's back would not be.
    if merge {
        bail!(
            "MERGE_NOT_SUPPORTED: --merge cannot be honoured. Merging two Clawket stores \
             requires remapping generated ids across every foreign-key edge and resolving \
             invariants that permit no automatic answer (one active plan per project, one \
             active cycle per unit, unique project cwd). Re-run without --merge to replace \
             the current database, after taking a backup of it. \
             (A CLI at v0.6.1 or older describes --merge as supported: that flag has never \
             reached a daemon — /restore did not exist until now — so nothing that worked \
             before stops working. Its help is corrected in the next CLI release.)"
        );
    }

    if !input.exists() {
        bail!(
            "BACKUP_ARCHIVE_NOT_FOUND: no such archive: {}",
            input.display()
        );
    }

    let (header, sqlite_bytes) = decode_archive(input)?;

    // Downgrade guard, same spirit as `db.rs`'s SCHEMA_DOWNGRADE_REFUSED: a
    // newer archive would put this binary in front of a schema it does not
    // know how to read.
    //
    // Distinct code from that one on purpose. They read alike but call for
    // different handling: the db.rs guard fires at startup over the daemon's
    // OWN store and the daemon does not come up, so the operator upgrades and
    // retries nothing. This one rejects a single request over a file the caller
    // chose — the daemon keeps serving, and the caller can pick another
    // archive. A client matching one code could not tell those apart.
    if header.schema_version > SCHEMA_VERSION_MAX {
        bail!(
            "ARCHIVE_SCHEMA_TOO_NEW: archive schema version {} > binary max {}. \
             Upgrade clawketd before restoring this archive.",
            header.schema_version,
            SCHEMA_VERSION_MAX
        );
    }

    // Stage next to the destination so the final move is a same-filesystem
    // rename (atomic) rather than a copy that can fail halfway.
    let parent = db_path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create db dir: {}", parent.display()))?;
    // pid + nonce. With the pid alone, two concurrent restores inside one daemon
    // staged to the same path: measured over 40 iterations per thread, 4 failed
    // outright at the rename, and a worse interleaving is possible in principle
    // — A validates its payload, B overwrites the staged file, A renames B's
    // bytes into place, so the archive that was checked is not the one installed.
    let staged = parent.join(format!(
        ".clawket-restore-{}.{}.tmp",
        std::process::id(),
        next_seq()
    ));
    // `create_new`, like the backup path, so a name collision fails loudly
    // instead of truncating someone else's staged payload. `next_seq()` restarts
    // at 0 with the process, so a leftover from a daemon killed mid-restore can
    // in principle share a name after pid reuse — rare, but silently overwriting
    // is not an acceptable way to lose that race.
    let _ = std::fs::remove_file(&staged);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
            .with_context(|| format!("stage restore payload: {}", staged.display()))?;
        f.write_all(&sqlite_bytes)
            .with_context(|| format!("write restore payload: {}", staged.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync restore payload: {}", staged.display()))?;
    }

    // Validate the staged file as a real database before it can replace
    // anything. Failure here must not leave the staging file behind.
    let staged_version = match read_schema_version(&staged) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            return Err(e);
        }
    };
    if staged_version != header.schema_version {
        let _ = std::fs::remove_file(&staged);
        bail!(
            "BACKUP_ARCHIVE_INVALID: header claims schema version {} but payload reports {}",
            header.schema_version,
            staged_version
        );
    }

    if dry_run {
        // Everything above was read-only with respect to the live database;
        // drop the staged copy and report what a real run would do.
        let _ = std::fs::remove_file(&staged);
        return Ok(RestoreResult {
            input: input.to_string_lossy().into_owned(),
            dry_run: true,
            restored: false,
            archive_schema_version: header.schema_version,
            current_schema_version,
            archive_created_at: header.created_at,
            archive_project_id: header.project_id,
            previous_db_backup: None,
            message: format!(
                "dry run: archive is valid (schema v{}); would replace {} (schema v{}). \
                 Nothing was changed.",
                header.schema_version,
                db_path.display(),
                current_schema_version
            ),
        });
    }

    // Keep the outgoing database. A structurally valid archive can still hold
    // the wrong data, and at this point the user has no other copy.
    //
    // Append rather than `with_extension`, which REPLACES: `CLAWKET_DB` is a
    // documented override, so a store at `clawket.db` would have had its backup
    // written to `clawket.pre-restore` — a name the user is not told about and
    // would not find. Appending keeps the copy beside the file it came from
    // whatever that file is called.
    let previous = {
        let mut n = db_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "db".to_string());
        n.push_str(".pre-restore");
        db_path.parent().unwrap_or(Path::new(".")).join(n)
    };
    let previous_reported = if db_path.exists() {
        // Clean up the staged payload on failure: the earlier error paths do,
        // and without it a failed copy leaves `.clawket-restore-*.tmp` sitting
        // in the data directory forever.
        if let Err(e) = std::fs::copy(db_path, &previous) {
            let _ = std::fs::remove_file(&staged);
            return Err(e).with_context(|| {
                format!("preserve current db before restore: {}", previous.display())
            });
        }
        Some(previous.to_string_lossy().into_owned())
    } else {
        None
    };

    std::fs::rename(&staged, db_path).with_context(|| {
        format!(
            "swap restored db into place: {} -> {}",
            staged.display(),
            db_path.display()
        )
    })?;

    // WAL/SHM belong to the file we just replaced; leaving them would let
    // SQLite reapply frames from the old database on top of the new one.
    for suffix in ["-wal", "-shm"] {
        let mut side = db_path.as_os_str().to_os_string();
        side.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(side));
    }

    Ok(RestoreResult {
        input: input.to_string_lossy().into_owned(),
        dry_run: false,
        restored: true,
        archive_schema_version: header.schema_version,
        current_schema_version,
        archive_created_at: header.created_at,
        archive_project_id: header.project_id,
        previous_db_backup: previous_reported,
        message: format!(
            "restored {} from archive (schema v{}). Restart clawketd — open connections \
             still point at the replaced file.",
            db_path.display(),
            header.schema_version
        ),
    })
}

/// Public wrapper so routes validate paths through the same rules.
pub fn resolve_output_path(raw: Option<&str>, now_ms: i64) -> Result<PathBuf> {
    match raw {
        Some(s) => sanitize_path(s, "output"),
        None => Ok(default_output_path(now_ms)),
    }
}

/// Public wrapper for the restore side.
pub fn resolve_input_path(raw: &str) -> Result<PathBuf> {
    sanitize_path(raw, "input")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_db(path: &Path, version: i64, marker: &str) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at INTEGER);
             CREATE TABLE marker (v TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_version (version, applied_at) VALUES (?1, 0)",
            [version],
        )
        .unwrap();
        conn.execute("INSERT INTO marker (v) VALUES (?1)", [marker])
            .unwrap();
    }

    fn read_marker(path: &Path) -> String {
        let conn = Connection::open(path).unwrap();
        conn.query_row("SELECT v FROM marker", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn backup_restore_round_trip_preserves_data() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("db.sqlite");
        make_db(&db, SCHEMA_VERSION_MAX, "original");

        let archive = tmp.path().join("out.clawketbak.gz");
        let res = backup(&db, &archive, None, SCHEMA_VERSION_MAX, 1234).unwrap();
        assert!(res.bytes > 0);
        assert!(archive.exists());

        // Mutate the live db so a successful restore is observable.
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute("UPDATE marker SET v = 'changed'", []).unwrap();
        }
        assert_eq!(read_marker(&db), "changed");

        let r = restore(&db, &archive, false, false, SCHEMA_VERSION_MAX).unwrap();
        assert!(r.restored);
        assert_eq!(r.archive_schema_version, SCHEMA_VERSION_MAX);
        assert_eq!(read_marker(&db), "original");
        // The outgoing database was preserved.
        assert!(db.with_extension("sqlite.pre-restore").exists());
    }

    #[test]
    fn dry_run_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("db.sqlite");
        make_db(&db, SCHEMA_VERSION_MAX, "original");

        let archive = tmp.path().join("out.clawketbak.gz");
        backup(&db, &archive, None, SCHEMA_VERSION_MAX, 1).unwrap();

        {
            let conn = Connection::open(&db).unwrap();
            conn.execute("UPDATE marker SET v = 'changed'", []).unwrap();
        }

        let r = restore(&db, &archive, false, true, SCHEMA_VERSION_MAX).unwrap();
        assert!(r.dry_run);
        assert!(!r.restored);
        // Live db untouched, no pre-restore copy, no staging leftovers.
        assert_eq!(read_marker(&db), "changed");
        assert!(!db.with_extension("sqlite.pre-restore").exists());
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".clawket-restore-")
            })
            .collect();
        assert!(leftovers.is_empty(), "staging file leaked");
    }

    #[test]
    fn corrupt_archive_is_rejected_and_db_survives() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("db.sqlite");
        make_db(&db, SCHEMA_VERSION_MAX, "original");

        // Not a gzip stream at all.
        let junk = tmp.path().join("junk.gz");
        std::fs::write(&junk, b"definitely not gzip").unwrap();
        let err = restore(&db, &junk, false, false, SCHEMA_VERSION_MAX).unwrap_err();
        assert!(
            err.to_string().starts_with("BACKUP_ARCHIVE_INVALID:"),
            "unexpected error: {err}"
        );
        assert_eq!(read_marker(&db), "original");

        // Valid gzip + magic + header, but a payload that is not a database.
        let bogus = tmp.path().join("bogus.gz");
        {
            let header = BackupHeader {
                schema_version: SCHEMA_VERSION_MAX,
                created_at: 0,
                daemon_version: "test".into(),
                project_id: None,
            };
            let hj = serde_json::to_vec(&header).unwrap();
            let f = std::fs::File::create(&bogus).unwrap();
            let mut enc = GzEncoder::new(f, Compression::default());
            enc.write_all(MAGIC).unwrap();
            enc.write_all(&(hj.len() as u32).to_le_bytes()).unwrap();
            enc.write_all(&hj).unwrap();
            enc.write_all(b"not a sqlite file").unwrap();
            enc.finish().unwrap();
        }
        let err = restore(&db, &bogus, false, false, SCHEMA_VERSION_MAX).unwrap_err();
        assert!(
            err.to_string().contains("BACKUP_ARCHIVE_INVALID")
                || err.to_string().contains("integrity_check"),
            "unexpected error: {err}"
        );
        assert_eq!(read_marker(&db), "original");
    }

    #[test]
    fn newer_schema_archive_is_refused() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.sqlite");
        make_db(&src, SCHEMA_VERSION_MAX + 1, "future");
        let archive = tmp.path().join("future.gz");
        backup(&src, &archive, None, SCHEMA_VERSION_MAX + 1, 0).unwrap();

        let db = tmp.path().join("db.sqlite");
        make_db(&db, SCHEMA_VERSION_MAX, "original");

        let err = restore(&db, &archive, false, false, SCHEMA_VERSION_MAX).unwrap_err();
        assert!(
            err.to_string().starts_with("ARCHIVE_SCHEMA_TOO_NEW:"),
            "unexpected error: {err}"
        );
        assert_eq!(read_marker(&db), "original");
    }

    #[test]
    fn merge_is_refused_with_a_reason() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("db.sqlite");
        make_db(&db, SCHEMA_VERSION_MAX, "original");
        let archive = tmp.path().join("a.gz");
        backup(&db, &archive, None, SCHEMA_VERSION_MAX, 0).unwrap();

        let err = restore(&db, &archive, true, false, SCHEMA_VERSION_MAX).unwrap_err();
        assert!(
            err.to_string().starts_with("MERGE_NOT_SUPPORTED:"),
            "unexpected error: {err}"
        );
        // Refused before any staging work.
        assert_eq!(read_marker(&db), "original");
    }

    #[test]
    fn relative_and_empty_paths_are_rejected() {
        assert!(sanitize_path("", "output").is_err());
        assert!(sanitize_path("relative/path.gz", "output").is_err());
        assert!(sanitize_path("/tmp/ok\0bad", "output").is_err());
        let p = sanitize_path("/tmp/./a/../b.gz", "output").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/b.gz"));
    }
}
