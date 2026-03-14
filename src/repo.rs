use anyhow::{Context, Result};
use git2::{Oid, Repository, Sort, TreeWalkMode, TreeWalkResult};
use globset::GlobSet;
use rustc_hash::FxHashMap;
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

    let mut commits = Vec::new();
    for id in walk {
        let oid = id?;
        let ts = repo.find_commit(oid)?.time().seconds();
        commits.push((oid, ts));
    }
    if commits.is_empty() {
        anyhow::bail!("No commits found — is this a valid git repository?");
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

/// Returns all `(commit_oid, blob_oid)` work pairs **and** a deduplicated
/// `blame_lookup` map: one `(commit_oid, file_path)` per unique blob OID,
/// sufficient for the blame phase without duplicating paths across commits.
#[allow(clippy::type_complexity)]
pub fn collect_work_items(
    repo_path: &Path,
    sampled: &[(Oid, i64)],
    extensions: &Option<Vec<String>>,
    include_set: &Option<GlobSet>,
    exclude_set: &Option<GlobSet>,
) -> Result<(Vec<WorkItem>, FxHashMap<Oid, (Oid, String)>)> {
    let repo = Repository::open(repo_path)?;
    let mut items = Vec::new();
    let mut blame_lookup: FxHashMap<Oid, (Oid, String)> = FxHashMap::default();

    for &(oid, _) in sampled {
        let before = items.len();
        let commit = repo.find_commit(oid)?;
        let tree = commit.tree()?;

        tree.walk(TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() != Some(git2::ObjectType::Blob) {
                return TreeWalkResult::Ok;
            }
            let name = match entry.name() {
                Some(n) => n,
                None => return TreeWalkResult::Ok,
            };
            if let Some(ref exts) = extensions {
                let matched = Path::new(name)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| exts.iter().any(|x| x.trim_start_matches('.') == e))
                    .unwrap_or(false);
                if !matched {
                    return TreeWalkResult::Ok;
                }
            }
            let full_path = format!("{dir}{name}");
            if let Some(ref inc) = include_set {
                if !inc.is_match(&full_path) {
                    return TreeWalkResult::Ok;
                }
            }
            if let Some(ref exc) = exclude_set {
                if exc.is_match(&full_path) {
                    return TreeWalkResult::Ok;
                }
            }
            let blob_oid = entry.id();
            items.push(WorkItem { commit_oid: oid, blob_oid });
            // Store one (commit_oid, path) per unique blob for the blame phase.
            blame_lookup.entry(blob_oid).or_insert_with(|| (oid, full_path));
            TreeWalkResult::Ok
        })?;

        trace!(
            "commit {} → {} files",
            &oid.to_string()[..8],
            items.len() - before
        );
    }

    debug!("{} (commit, file) work items collected", items.len());
    Ok((items, blame_lookup))
}
