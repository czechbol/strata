use anyhow::{Context, Result};
use git2::{Oid, Repository, Sort};
use globset::GlobSet;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info, trace, warn};

use crate::ssh::make_fetch_options;
use crate::types::WorkItem;

fn fnv1a_u32(s: &str) -> u32 {
    let mut h: u32 = 2166136261;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

pub fn repo_name(url: &str) -> String {
    url.trim_end_matches('/')
        .trim_end_matches(".git")
        .split('/')
        .next_back()
        .unwrap_or("repo")
        .to_string()
}

fn repo_clone_path(url: &str) -> PathBuf {
    let name = repo_name(url);
    let hash = fnv1a_u32(url);
    env::temp_dir()
        .join("strata")
        .join("downloads")
        .join(format!("{}-{:08x}", name, hash))
}

pub fn ensure_repo(url: &str, ssh_key: Option<PathBuf>) -> Result<PathBuf> {
    let local = Path::new(url);
    if local.exists() && local.is_dir() && Repository::open(local).is_ok() {
        info!("Using local repo at {url}");
        return Ok(local.to_path_buf());
    }

    let path = repo_clone_path(url);
    if path.exists() {
        let valid = Repository::open(&path)
            .map(|r| !r.is_empty().unwrap_or(true))
            .unwrap_or(false);
        if valid {
            info!("Using cached clone at {}", path.display());
            return Ok(path);
        }
        // Partial/failed clone left a directory behind — remove and retry
        warn!("Cached path exists but is empty or invalid, re-cloning");
        fs::remove_dir_all(&path)?;
    }

    info!("Cloning {url}");
    debug!("Destination: {}", path.display());

    fs::create_dir_all(path.parent().context("invalid clone path")?)?;

    if url.starts_with("http://") || url.starts_with("https://") {
        // Shell out to git for HTTPS: handles public repos and credential helpers
        // without being affected by libgit2 reading insteadOf URL rewrites.
        let status = std::process::Command::new("git")
            .args(["clone", "--", url, &path.to_string_lossy()])
            .status()
            .context("failed to spawn git clone")?;
        anyhow::ensure!(status.success(), "Failed to clone {url}");
    } else {
        git2::build::RepoBuilder::new()
            .fetch_options(make_fetch_options(url, ssh_key))
            .clone(url, &path)
            .with_context(|| format!("Failed to clone {url}"))?;
    }
    info!("Clone complete");
    Ok(path)
}

/// Returns the root commit (0 parents) reached by following first-parent links from HEAD.
fn find_first_parent_root(repo: &Repository) -> Result<Oid> {
    let mut current = repo
        .head()
        .and_then(|r| r.peel_to_commit())
        .map(|c| c.id())
        .unwrap_or_else(|_| Oid::zero());
    loop {
        let commit = repo.find_commit(current)?;
        if commit.parent_count() == 0 {
            return Ok(current);
        }
        current = commit.parent_id(0)?;
    }
}

/// Returns OIDs of commits whose first-parent chain does NOT lead to `main_root`.
///
/// These are commits from unrelated histories that were merged in via
/// `--allow-unrelated-histories` (e.g. a module developed as a separate repo).
/// Their trees only contain that project's files, so sampling them produces
/// dramatic drops in the chart.
///
/// Uses memoised traversal: each commit's first-parent chain is walked once and
/// the result (foreign / native) is cached for all commits in that chain.
fn find_foreign_oids(
    repo: &Repository,
    commits: &[(Oid, i64)],
    main_root: Oid,
) -> Result<FxHashSet<Oid>> {
    let mut memo: FxHashMap<Oid, bool> = FxHashMap::default(); // true = foreign
    memo.insert(main_root, false);
    let mut foreign: FxHashSet<Oid> = FxHashSet::default();

    for &(oid, _) in commits {
        if memo.contains_key(&oid) {
            if memo[&oid] {
                foreign.insert(oid);
            }
            continue;
        }

        // Walk the first-parent chain until we reach a memoised commit or a root,
        // accumulating unresolved commits in `chain`.
        let mut chain: Vec<Oid> = Vec::new();
        let mut current = oid;

        loop {
            if let Some(&is_foreign) = memo.get(&current) {
                for &c in &chain {
                    memo.insert(c, is_foreign);
                    if is_foreign {
                        foreign.insert(c);
                    }
                }
                break;
            }

            chain.push(current);

            let commit = repo.find_commit(current)?;
            if commit.parent_count() == 0 {
                // Root commit — foreign if it isn't the known main root.
                let is_foreign = current != main_root;
                for &c in &chain {
                    memo.insert(c, is_foreign);
                    if is_foreign {
                        foreign.insert(c);
                    }
                }
                break;
            }

            current = commit.parent_id(0)?;
        }
    }

    Ok(foreign)
}

pub fn get_commit_list(repo_path: &Path, first_parent: bool) -> Result<Vec<(Oid, i64)>> {
    let repo = Repository::open(repo_path)?;
    let mut walk = repo.revwalk()?;
    walk.set_sorting(Sort::TIME)?;
    if first_parent {
        walk.simplify_first_parent()?;
    }

    // push_head() fails when HEAD points to an unborn or non-existent branch
    // (e.g. a repo whose default branch is "master" but was cloned expecting "main").
    // Fall back to pushing all local + remote-tracking branch tips.
    if walk.push_head().is_err() {
        debug!("HEAD unresolvable; falling back to all branch tips");
        for branch in repo.branches(Some(git2::BranchType::Local))? {
            let (b, _) = branch?;
            if let Some(oid) = b.get().target() {
                let _ = walk.push(oid);
            }
        }
        for branch in repo.branches(Some(git2::BranchType::Remote))? {
            let (b, _) = branch?;
            if let Some(oid) = b.get().target() {
                let _ = walk.push(oid);
            }
        }
    }

    let mut commits: Vec<(Oid, i64)> = Vec::new();
    let mut subtree_squash: FxHashSet<Oid> = FxHashSet::default();
    for id in walk {
        let oid = id?;
        let commit = repo.find_commit(oid)?;
        let ts = commit.time().seconds();
        // Detect git-subtree squash imports: their tree contains only the
        // subtree's files (e.g. hiredis, jemalloc), not the full codebase.
        if !first_parent && commit.message().unwrap_or("").starts_with("Squashed '") {
            subtree_squash.insert(oid);
        }
        commits.push((oid, ts));
    }
    if commits.is_empty() {
        anyhow::bail!("No commits found — is this a valid git repository?");
    }

    // When doing a full DAG walk, filter out two classes of commits that produce
    // near-zero drops because their trees are sparse relative to the mainline:
    //   1. git-subtree squash commits (detected above by message prefix)
    //   2. Commits from unrelated histories merged in via --allow-unrelated-histories
    //      (detected by their first-parent chain leading to a different root)
    if !first_parent {
        let main_root = find_first_parent_root(&repo)?;
        let foreign = find_foreign_oids(&repo, &commits, main_root)?;
        let before = commits.len();
        commits.retain(|(oid, _)| !foreign.contains(oid) && !subtree_squash.contains(oid));
        let removed = before - commits.len();
        if removed > 0 {
            debug!(
                "Filtered {removed} unrelated-history/subtree-squash commits \
                 ({} remain)",
                commits.len()
            );
        }
    }

    commits.sort_by_key(|&(_, ts)| ts);
    debug!("Found {} commits in history", commits.len());
    Ok(commits)
}

pub fn sample_commits(commits: Vec<(Oid, i64)>, n: usize) -> Vec<(Oid, i64)> {
    let len = commits.len();
    if n == 0 || len <= n {
        return commits;
    }
    if n == 1 {
        return vec![commits[len / 2]];
    }
    let sampled: Vec<_> = (0..n).map(|i| commits[i * (len - 1) / (n - 1)]).collect();
    let span_years = {
        let (t0, t1) = (sampled.first().unwrap().1, sampled.last().unwrap().1);
        (t1 - t0) as f64 / (365.25 * 86400.0)
    };
    debug!(
        "Sampling {n}/{len} commits spanning {span_years:.1} years \
         (one every ~{:.0} commits)",
        len as f64 / n as f64
    );
    sampled
}

/// Parse one line of `git ls-tree -r` output into `(blob_oid, path)`.
/// Format: `<mode> <type> <sha>\t<path>`
fn parse_ls_tree_line(line: &[u8]) -> Option<(Oid, String)> {
    let tab = line.iter().position(|&b| b == b'\t')?;
    let meta = &line[..tab];
    let path = std::str::from_utf8(&line[tab + 1..]).ok()?;
    if path.is_empty() {
        return None;
    }
    // meta = "<mode> <type> <sha>" — find the two spaces
    let sp1 = meta.iter().position(|&b| b == b' ')?;
    let sp2 = meta[sp1 + 1..].iter().position(|&b| b == b' ')? + sp1 + 1;
    if &meta[sp1 + 1..sp2] != b"blob" {
        return None;
    }
    let sha = std::str::from_utf8(&meta[sp2 + 1..]).ok()?;
    let blob_oid = Oid::from_str(sha).ok()?;
    Some((blob_oid, path.to_string()))
}

/// Shell out to `git ls-tree -r` to list all blobs in a commit tree.
/// Same approach as blame: subprocess is orders of magnitude faster than
/// libgit2's tree walk under parallel load due to ODB lock contention.
fn ls_tree_blobs(repo_path: &Path, commit_oid: Oid) -> Vec<(Oid, String)> {
    let output = std::process::Command::new("git")
        .args([
            "-C",
            &repo_path.to_string_lossy(),
            "ls-tree",
            "-r",
            "--full-tree",
            &commit_oid.to_string(),
        ])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default();

    output
        .split(|&b| b == b'\n')
        .filter_map(parse_ls_tree_line)
        .collect()
}

/// Returns all `(commit_oid, blob_oid)` work pairs **and** a deduplicated
/// `blame_lookup` map: one `(commit_oid, file_path)` per unique blob OID,
/// sufficient for the blame phase without duplicating paths across commits.
///
/// Uses `git ls-tree` subprocesses (not libgit2) to avoid ODB lock contention
/// under parallel load. Runs inside `pool` so `-j` applies to both this phase
/// and the subsequent blame phase.
#[allow(clippy::type_complexity)]
pub fn collect_work_items(
    repo_path: &Path,
    sampled: &[(Oid, i64)],
    extensions: &Option<Vec<String>>,
    include_set: &Option<GlobSet>,
    exclude_set: &Option<GlobSet>,
    pool: &rayon::ThreadPool,
) -> Result<(Vec<WorkItem>, FxHashMap<Oid, (Oid, String)>)> {
    let per_commit: Vec<Vec<(Oid, Oid, String)>> = pool.install(|| {
        sampled
            .par_iter()
            .map(|&(oid, _)| {
                let entries: Vec<(Oid, Oid, String)> = ls_tree_blobs(repo_path, oid)
                    .into_iter()
                    .filter(|(_, path)| {
                        if let Some(ref exts) = extensions {
                            let ext = Path::new(path)
                                .extension()
                                .and_then(|e| e.to_str())
                                .unwrap_or("");
                            if !exts.iter().any(|x| x.trim_start_matches('.') == ext) {
                                return false;
                            }
                        }
                        if let Some(ref inc) = include_set {
                            if !inc.is_match(path) {
                                return false;
                            }
                        }
                        if let Some(ref exc) = exclude_set {
                            if exc.is_match(path) {
                                return false;
                            }
                        }
                        true
                    })
                    .map(|(blob_oid, path)| (blob_oid, oid, path))
                    .collect();
                trace!("commit {} → {} files", &oid.to_string()[..8], entries.len());
                entries
            })
            .collect()
    });

    // Merge sequentially to build items and deduplicated blame_lookup.
    let mut items = Vec::new();
    let mut blame_lookup: FxHashMap<Oid, (Oid, String)> = FxHashMap::default();
    for commit_entries in per_commit {
        for (blob_oid, commit_oid, path) in commit_entries {
            items.push(WorkItem { commit_oid, blob_oid });
            // Store one (commit_oid, path) per unique blob for the blame phase.
            blame_lookup.entry(blob_oid).or_insert_with(|| (commit_oid, path));
        }
    }

    debug!("{} (commit, file) work items collected", items.len());
    Ok((items, blame_lookup))
}
