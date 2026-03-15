use anyhow::{Context, Result};
use git2::{Oid, Repository, Sort};
use globset::GlobSet;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::RefCell;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
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

/// A persistent `git cat-file --batch` subprocess used to read tree objects
/// without per-commit spawn overhead. One instance lives per rayon thread via
/// `thread_local!`, amortising git startup across all commits on that thread.
struct CatFile {
    _child: std::process::Child,
    stdin: BufWriter<std::process::ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
}

impl CatFile {
    fn start(repo_path: &Path) -> Option<Self> {
        let mut child = std::process::Command::new("git")
            .args(["-C", &repo_path.to_string_lossy(), "cat-file", "--batch"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let stdin = BufWriter::new(child.stdin.take()?);
        let stdout = BufReader::new(child.stdout.take()?);
        Some(Self { _child: child, stdin, stdout })
    }

    /// Recursively walk a tree object, appending `(blob_oid, path)` to `out`.
    /// Binary tree format per entry: `<mode> <name>\0<20-byte-sha>`.
    fn walk_tree(&mut self, tree_oid: Oid, prefix: &str, out: &mut Vec<(Oid, String)>) {
        if writeln!(self.stdin, "{tree_oid}").is_err() { return; }
        if self.stdin.flush().is_err() { return; }

        // Header: "<sha> <type> <size>\n"  or  "<sha> missing\n"
        let mut header = String::new();
        if self.stdout.read_line(&mut header).is_err() { return; }
        let mut parts = header.split_ascii_whitespace();
        parts.next(); // sha
        let obj_type = parts.next().unwrap_or("");
        let size: usize = match parts.next().and_then(|s| s.parse().ok()) {
            Some(n) if obj_type == "tree" => n,
            _ => return, // missing or not a tree; stream is still clean (no data follows)
        };

        let mut data = vec![0u8; size];
        if Read::read_exact(&mut self.stdout, &mut data).is_err() { return; }
        let mut nl = [0u8; 1];
        let _ = Read::read_exact(&mut self.stdout, &mut nl); // consume trailing newline

        // Parse entries: "<mode> <name>\0<20-byte-sha>" ...
        let mut i = 0;
        while i < data.len() {
            let Some(sp) = data[i..].iter().position(|&b| b == b' ') else { break };
            let sp = i + sp;
            let mode = &data[i..sp];

            let Some(nul) = data[sp + 1..].iter().position(|&b| b == 0) else { break };
            let nul = sp + 1 + nul;
            let name = match std::str::from_utf8(&data[sp + 1..nul]) {
                Ok(n) => n,
                Err(_) => { i = nul + 21; continue; }
            };
            if nul + 21 > data.len() { break; }
            let entry_oid = match Oid::from_bytes(&data[nul + 1..nul + 21]) {
                Ok(id) => id,
                Err(_) => { i = nul + 21; continue; }
            };
            i = nul + 21;

            if mode == b"40000" || mode == b"040000" {
                self.walk_tree(entry_oid, &format!("{prefix}{name}/"), out);
            } else if mode != b"160000" {
                // blob (100644 / 100755 / 120000); skip gitlinks (160000)
                out.push((entry_oid, format!("{prefix}{name}")));
            }
        }
    }
}

/// Returns all `(commit_oid, blob_oid)` work pairs **and** a deduplicated
/// `blame_lookup` map: one `(commit_oid, file_path)` per unique blob OID,
/// sufficient for the blame phase without duplicating paths across commits.
///
/// Uses one persistent `git cat-file --batch` process per rayon thread so
/// git startup cost is paid once per thread, not once per commit. Runs inside
/// `pool` so `-j` controls parallelism for both this phase and blame.
#[allow(clippy::type_complexity)]
pub fn collect_work_items(
    repo_path: &Path,
    sampled: &[(Oid, i64)],
    extensions: &Option<Vec<String>>,
    include_set: &Option<GlobSet>,
    exclude_set: &Option<GlobSet>,
    pool: &rayon::ThreadPool,
) -> Result<(Vec<WorkItem>, FxHashMap<Oid, (Oid, String)>)> {
    // Single-threaded: resolve tree OID for each commit (reads only the commit
    // object — a few hundred bytes — so this is very fast).
    let commit_trees: Vec<(Oid, Oid)> = {
        let repo = Repository::open(repo_path)?;
        sampled
            .iter()
            .filter_map(|&(oid, _)| Some((oid, repo.find_commit(oid).ok()?.tree_id())))
            .collect()
    };

    thread_local! {
        static CAT_FILE: RefCell<Option<CatFile>> = RefCell::new(None);
    }

    let per_commit: Vec<Vec<(Oid, Oid, String)>> = pool.install(|| {
        commit_trees
            .par_iter()
            .map(|&(commit_oid, tree_oid)| {
                CAT_FILE.with(|cell| {
                    let mut opt = cell.borrow_mut();
                    if opt.is_none() {
                        *opt = CatFile::start(repo_path);
                    }
                    let Some(cat_file) = opt.as_mut() else { return vec![] };

                    let mut blobs = Vec::new();
                    cat_file.walk_tree(tree_oid, "", &mut blobs);

                    let entries = blobs
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
                                if !inc.is_match(path) { return false; }
                            }
                            if let Some(ref exc) = exclude_set {
                                if exc.is_match(path) { return false; }
                            }
                            true
                        })
                        .map(|(blob_oid, path)| (blob_oid, commit_oid, path))
                        .collect::<Vec<_>>();
                    trace!("commit {} → {} files", &commit_oid.to_string()[..8], entries.len());
                    entries
                })
            })
            .collect()
    });

    // Merge sequentially to build items and deduplicated blame_lookup.
    let mut items = Vec::new();
    let mut blame_lookup: FxHashMap<Oid, (Oid, String)> = FxHashMap::default();
    for commit_entries in per_commit {
        for (blob_oid, commit_oid, path) in commit_entries {
            items.push(WorkItem { commit_oid, blob_oid });
            blame_lookup.entry(blob_oid).or_insert_with(|| (commit_oid, path));
        }
    }

    debug!("{} (commit, file) work items collected", items.len());
    Ok((items, blame_lookup))
}
