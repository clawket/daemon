//! Thin wrapper around `git` CLI for envelope auto-fill (RL-U3-08 / LM-61).
//!
//! Resolves a logical `target_repo` token (e.g. `"daemon"`) to a working
//! tree among the registered project cwds, then runs `git -C <path>
//! rev-parse HEAD` to capture the planned SHA. Returns `None` (treated as
//! a soft warning by callers) when:
//!   - no project cwd basename matches the token
//!   - the matched path is not a git repo
//!   - `git` is not installed or `rev-parse` exits non-zero
//!
//! No panics on git failure — callers downgrade to `planned_sha=null` and
//! attach a `planned_sha_warning` field instead.

use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::process::Command;

/// Find a project cwd whose basename matches `target_repo` and return its
/// HEAD SHA. The lookup is exact basename match, not substring.
pub fn resolve_target_repo_head(conn: &Connection, target_repo: &str) -> Option<String> {
    let path = resolve_target_repo_path(conn, target_repo)?;
    read_head_sha(&path)
}

/// Find the on-disk working tree for a `target_repo` token by basename match
/// against registered project cwds. The token may include an `@HEAD` suffix
/// (or any other ref-like suffix), which is stripped before lookup.
pub fn resolve_target_repo_path(conn: &Connection, target_repo: &str) -> Option<PathBuf> {
    let token = target_repo.trim();
    if token.is_empty() {
        return None;
    }
    let token = token.split('@').next().unwrap_or(token);
    if token.is_empty() {
        return None;
    }
    let candidates = list_cwds_with_basename(conn, token).ok()?;
    candidates.into_iter().find(|p| p.exists())
}

/// Run `git rev-parse HEAD` inside `path`. Returns `None` on any failure.
pub fn current_head(path: &std::path::Path) -> Option<String> {
    read_head_sha(path)
}

/// `git diff --name-only <base>..HEAD` inside `path`. Returns `None` on any
/// failure (bad sha, not a repo, git missing). Empty list when there are no
/// changes.
pub fn diff_files(path: &std::path::Path, base_sha: &str, head_sha: &str) -> Option<Vec<String>> {
    if base_sha.is_empty() || head_sha.is_empty() {
        return None;
    }
    let range = format!("{}..{}", base_sha, head_sha);
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["diff", "--name-only", &range])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(
        text.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

fn list_cwds_with_basename(conn: &Connection, basename: &str) -> rusqlite::Result<Vec<PathBuf>> {
    let mut stmt = conn.prepare("SELECT cwd FROM project_cwds")?;
    let rows = stmt.query_map(params![], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        let cwd = r?;
        let pb = PathBuf::from(&cwd);
        if pb.file_name().and_then(|n| n.to_str()) == Some(basename) {
            out.push(pb);
        }
    }
    Ok(out)
}

fn read_head_sha(path: &std::path::Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::repo::projects;
    use std::process::Command as Cmd;

    fn init_repo_and_commit(path: &std::path::Path) -> String {
        Cmd::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap();
        Cmd::new("git")
            .args(["-C"])
            .arg(path)
            .args(["config", "user.email", "test@example.com"])
            .status()
            .unwrap();
        Cmd::new("git")
            .args(["-C"])
            .arg(path)
            .args(["config", "user.name", "Test"])
            .status()
            .unwrap();
        std::fs::write(path.join("README"), "hello").unwrap();
        Cmd::new("git")
            .args(["-C"])
            .arg(path)
            .args(["add", "."])
            .status()
            .unwrap();
        Cmd::new("git")
            .args(["-C"])
            .arg(path)
            .args(["commit", "-q", "-m", "init"])
            .status()
            .unwrap();
        let out = Cmd::new("git")
            .args(["-C"])
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn resolves_head_when_basename_matches_registered_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().join("daemon");
        std::fs::create_dir_all(&repo_root).unwrap();
        let expected = init_repo_and_commit(&repo_root);

        let db_path = dir.path().join("test.db");
        let mut db = Db::open(&db_path).unwrap();
        projects::create(
            &mut db.conn,
            projects::CreateInput {
                name: "P",
                description: None,
                cwd: Some(repo_root.to_str().unwrap()),
                key: None,
            },
        )
        .unwrap();

        let sha = resolve_target_repo_head(&db.conn, "daemon").unwrap();
        assert_eq!(sha, expected);
    }

    #[test]
    fn returns_none_for_unknown_basename() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("test.db")).unwrap();
        assert!(resolve_target_repo_head(&db.conn, "nonexistent-repo").is_none());
    }

    #[test]
    fn strips_at_head_suffix() {
        // Even with no real repo, parsing should not panic.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("test.db")).unwrap();
        assert!(resolve_target_repo_head(&db.conn, "daemon@HEAD").is_none());
    }

    #[test]
    fn ignores_empty_token() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("test.db")).unwrap();
        assert!(resolve_target_repo_head(&db.conn, "").is_none());
        assert!(resolve_target_repo_head(&db.conn, "   ").is_none());
    }
}
