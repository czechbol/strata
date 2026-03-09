use git2::Oid;
use rustc_hash::FxHashMap;
use std::path::Path;
use tracing::warn;

// Shell out to the system git binary — orders of magnitude faster than libgit2's blame_file().
//
// --porcelain emits commit metadata only on the FIRST occurrence of each commit
// (vs --line-porcelain which repeats it for every line), so output is much smaller
// for files where many consecutive lines share the same commit.
//
// GIT_CONFIG_NOSYSTEM / GIT_CONFIG_GLOBAL shave per-process startup time across
// thousands of invocations by skipping config file reads that don't affect blame.
pub fn spawn_blame(
    repo_path: &Path,
    commit_oid: Oid,
    file_path: &str,
) -> Option<std::process::Child> {
    std::process::Command::new("git")
        .args([
            "-C", &repo_path.to_string_lossy(),
            "blame", "--porcelain",
            &commit_oid.to_string(),
            "--", file_path,
        ])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| warn!("git spawn failed for {file_path}: {e}"))
        .ok()
}

pub fn parse_blame_output(stdout: &[u8], blob_oid: Oid, cache: &sled::Db) -> Vec<(i64, String)> {
    // Parse --porcelain: commit metadata appears once per commit, keyed by the 40-char sha
    // that starts each hunk header. "author" and "author-time" lines are cached per sha;
    // "\t" lines emit one (timestamp, author) pair.
    let mut sha_ts: FxHashMap<[u8; 40], i64> = FxHashMap::default();
    let mut sha_author: FxHashMap<[u8; 40], String> = FxHashMap::default();
    let mut current_sha = [0u8; 40];
    let mut current_ts: i64 = 0;
    let mut current_author = String::new();
    let mut lines = Vec::new();

    for line in stdout.split(|&b| b == b'\n') {
        // Hunk header: "<sha40> <orig> <final> [<count>]"
        if line.len() >= 41 && line[40] == b' ' && line[..40].iter().all(|b| b.is_ascii_hexdigit()) {
            current_sha = line[..40].try_into().unwrap();
            current_ts = sha_ts.get(&current_sha).copied().unwrap_or(0);
            current_author = sha_author.get(&current_sha).cloned().unwrap_or_default();
        } else if let Some(rest) = line.strip_prefix(b"author ") {
            // "author " (with space) is the blame author name line — distinct from "author-mail", "author-time", etc.
            if let Ok(name) = std::str::from_utf8(rest) {
                let name = name.trim().to_string();
                sha_author.insert(current_sha, name.clone());
                current_author = name;
            }
        } else if let Some(rest) = line.strip_prefix(b"author-time ") {
            let ts = std::str::from_utf8(rest)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            sha_ts.insert(current_sha, ts);
            current_ts = ts;
        } else if line.starts_with(b"\t") {
            lines.push((current_ts, current_author.clone()));
        }
    }

    if let Ok(encoded) = bincode::serialize(&lines) {
        let _ = cache.insert(blob_oid.as_bytes(), encoded.as_slice());
    }
    lines
}
