mod blame;
mod period;
mod repo;
mod serve;
mod ssh;
mod types;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use git2::{Oid, Repository};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info};
use tracing_subscriber::{fmt, EnvFilter};

use blame::{parse_blame_output, spawn_blame};
use period::{get_version_tags, period_int_to_string, ts_to_period_int};
use repo::{collect_work_items, ensure_repo, get_commit_list, repo_name, sample_commits};
use types::{OutputData, SeriesPoint, WorkItem};

type BlobHists = FxHashMap<Oid, (FxHashMap<i32, u64>, FxHashMap<String, u64>)>;
type PeriodAgg = FxHashMap<(Oid, i32), u64>;
type AuthorAgg = FxHashMap<(Oid, String), u64>;

#[derive(Parser, Debug)]
#[command(name = "strata", about = "Git code archaeology — fast parallel blame aggregator")]
struct Cli {
    /// Increase log verbosity (-v info, -vv debug, -vvv trace)
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Analyze repositories and write data files
    Process(ProcessArgs),
    /// Serve the web UI with embedded assets
    Serve(ServeArgs),
}

#[derive(clap::ValueEnum, Debug, Clone, PartialEq)]
enum Granularity {
    Quarter,
    Year,
}

impl std::fmt::Display for Granularity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Granularity::Quarter => write!(f, "quarter"),
            Granularity::Year => write!(f, "year"),
        }
    }
}

#[derive(Parser, Debug)]
struct ProcessArgs {
    #[arg(short = 'r', long, help = "Repository URL or local path")]
    repo: String,

    #[arg(short = 's', long, default_value = "100", help = "Commits to sample (0 = all)")]
    samples: usize,

    #[arg(short = 'e', long, help = "File extensions to include, comma-separated (e.g. .py,.rs)")]
    extensions: Option<String>,

    #[arg(short = 'g', long, default_value = "quarter", help = "Granularity: quarter or year")]
    granularity: Granularity,

    #[arg(short = 'o', long, default_value = "web/data", help = "Output directory for data files")]
    output_dir: PathBuf,

    /// SSH private key to use for authentication (overrides agent and key discovery)
    #[arg(short = 'k', long = "key")]
    ssh_key: Option<PathBuf>,

    /// Ignore cached blame results and recompute from scratch
    #[arg(long = "no-cache")]
    no_cache: bool,

    /// Minimum fraction of lines (0.0–1.0) that top authors must cover before bucketing the rest as "other"
    #[arg(long = "author-threshold", default_value = "0.80")]
    author_threshold: f64,

    #[arg(long, help = "Glob patterns to include, comma-separated (e.g. 'src/**,lib/**')")]
    include: Option<String>,

    #[arg(long, help = "Glob patterns to exclude, comma-separated (e.g. 'tests/**,vendor/**')")]
    exclude: Option<String>,

    /// Maximum number of parallel blame processes (default: 8; raise to go faster at the cost of CPU/IO)
    #[arg(short = 'j', long = "jobs", default_value = "8")]
    jobs: usize,
}

#[derive(Parser, Debug)]
struct ServeArgs {
    /// Directory containing generated data files
    #[arg(short = 'd', long, default_value = "web/data")]
    dir: PathBuf,

    /// Host address to bind
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Port to listen on
    #[arg(short = 'p', long, default_value = "8080")]
    port: u16,
}

fn build_glob_set(patterns: Option<String>) -> Result<Option<globset::GlobSet>> {
    let Some(raw) = patterns else { return Ok(None) };
    let mut builder = globset::GlobSetBuilder::new();
    for pat in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        builder.add(globset::Glob::new(pat)?);
    }
    Ok(Some(builder.build()?))
}

fn init_tracing(verbose: u8) {
    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("strata={level}")));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_level(true)
        .compact()
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Command::Serve(args) => {
            return serve::serve(args.dir, args.host, args.port).await;
        }
        Command::Process(args) => run_process(args).await?,
    }

    Ok(())
}

fn build_blame_pool(jobs: usize) -> Result<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .build()
        .context("failed to create blame thread pool")
}

fn run_blame_phase(
    blame_lookup: &FxHashMap<Oid, (Oid, String)>,
    repo_path: &Path,
    no_cache: bool,
    cache: &sled::Db,
    pool: &rayon::ThreadPool,
    pb: &ProgressBar,
    yearly: bool,
    ignore_revs: Option<&Path>,
) -> (BlobHists, u64) {
    let cache_hits = std::sync::atomic::AtomicU64::new(0);
    let blob_hists = pool.install(|| {
        blame_lookup
            .par_iter()
            .map(|(&blob_oid, (commit_oid, file_path))| {
                let lines = if !no_cache {
                    cache
                        .get(blob_oid.as_bytes())
                        .ok()
                        .flatten()
                        .and_then(|v| bincode::deserialize::<Vec<(i64, String)>>(&v).ok())
                        .inspect(|_| {
                            cache_hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        })
                } else {
                    None
                }
                .unwrap_or_else(|| {
                    let lines = spawn_blame(repo_path, *commit_oid, file_path, ignore_revs)
                        .and_then(|child| child.wait_with_output().ok())
                        .filter(|o| o.status.success())
                        .map(|o| parse_blame_output(&o.stdout))
                        .unwrap_or_default();
                    if let Ok(encoded) = bincode::serialize(&lines) {
                        let _ = cache.insert(blob_oid.as_bytes(), encoded.as_slice());
                    }
                    lines
                });
                pb.inc(1);
                let mut period_hist: FxHashMap<i32, u64> = FxHashMap::default();
                let mut author_hist: FxHashMap<String, u64> = FxHashMap::default();
                for (lts, author) in lines {
                    *period_hist.entry(ts_to_period_int(lts, yearly)).or_insert(0) += 1;
                    *author_hist.entry(author).or_insert(0) += 1;
                }
                (blob_oid, (period_hist, author_hist))
            })
            .collect()
    });
    let hits = cache_hits.load(std::sync::atomic::Ordering::Relaxed);
    (blob_hists, hits)
}

fn aggregate(items: &[WorkItem], blob_hists: &BlobHists) -> (PeriodAgg, AuthorAgg) {
    let mut agg: PeriodAgg = FxHashMap::default();
    let mut author_agg: AuthorAgg = FxHashMap::default();
    for item in items {
        let Some((period_hist, author_hist)) = blob_hists.get(&item.blob_oid) else { continue };
        for (&period, &count) in period_hist {
            *agg.entry((item.commit_oid, period)).or_insert(0) += count;
        }
        for (author, &count) in author_hist {
            *author_agg.entry((item.commit_oid, author.clone())).or_insert(0) += count;
        }
    }
    (agg, author_agg)
}

fn select_authors(
    author_agg: &AuthorAgg,
    sampled: &[(Oid, i64)],
    threshold: f64,
) -> (Vec<String>, bool, FxHashMap<Oid, u64>) {
    let active_authors: Vec<String> = if let Some(&(last_oid, _)) = sampled.last() {
        let mut head_counts: Vec<(String, u64)> = author_agg
            .iter()
            .filter(|((oid, _), _)| *oid == last_oid)
            .map(|((_, author), &count)| (author.clone(), count))
            .collect();
        head_counts.sort_by(|a, b| b.1.cmp(&a.1));
        let total: u64 = head_counts.iter().map(|(_, c)| c).sum();
        let target = (total as f64 * threshold) as u64;
        let mut cumulative = 0u64;
        let mut active = Vec::new();
        for (author, count) in head_counts {
            active.push(author);
            cumulative += count;
            if cumulative >= target {
                break;
            }
        }
        active
    } else {
        Vec::new()
    };

    let active_author_set: FxHashSet<String> = active_authors.iter().cloned().collect();
    let has_other = sampled.last().is_some_and(|&(last_oid, _)| {
        author_agg.keys().any(|(oid, a)| *oid == last_oid && !active_author_set.contains(a))
    });

    let mut commit_other: FxHashMap<Oid, u64> = FxHashMap::default();
    if has_other {
        for ((oid, author), &count) in author_agg {
            if !active_author_set.contains(author) {
                *commit_other.entry(*oid).or_insert(0) += count;
            }
        }
    }

    (active_authors, has_other, commit_other)
}

#[allow(clippy::too_many_arguments)]
fn build_series(
    sampled: &[(Oid, i64)],
    agg: &PeriodAgg,
    author_agg: &AuthorAgg,
    active_authors: &[String],
    has_other: bool,
    commit_other: &FxHashMap<Oid, u64>,
    all_period_ints: &[i32],
    repo: &Repository,
) -> Vec<SeriesPoint> {
    sampled
        .iter()
        .map(|&(oid, ts)| {
            let (summary, author) = repo
                .find_commit(oid)
                .ok()
                .map(|c| (
                    c.summary().unwrap_or("").to_string(),
                    c.author().name().unwrap_or("unknown").to_string(),
                ))
                .unwrap_or_default();
            let counts: Vec<(usize, u64)> = all_period_ints
                .iter()
                .enumerate()
                .filter_map(|(i, &p)| {
                    let c = agg.get(&(oid, p)).copied().unwrap_or(0);
                    if c > 0 { Some((i, c)) } else { None }
                })
                .collect();
            let total: u64 = counts.iter().map(|(_, c)| c).sum();
            let mut author_counts: Vec<(usize, u64)> = active_authors
                .iter()
                .enumerate()
                .filter_map(|(i, a)| {
                    let c = author_agg.get(&(oid, a.clone())).copied().unwrap_or(0);
                    if c > 0 { Some((i, c)) } else { None }
                })
                .collect();
            if has_other {
                let c = commit_other.get(&oid).copied().unwrap_or(0);
                if c > 0 { author_counts.push((active_authors.len(), c)); }
            }
            SeriesPoint { ts, total, counts, author_counts, summary, author }
        })
        .collect()
}

fn write_output(output: &OutputData, output_dir: &Path, name: &str) -> Result<()> {
    let out_path = output_dir.join(format!("{name}.msgpack"));
    fs::write(&out_path, rmp_serde::to_vec_named(output)?)?;
    info!("Written {}", out_path.display());

    let repos_path = output_dir.join("repos.json");
    let mut repos: Vec<String> = if repos_path.exists() {
        serde_json::from_str(&fs::read_to_string(&repos_path)?).unwrap_or_default()
    } else {
        Vec::new()
    };
    if !repos.iter().any(|r| r == name) {
        repos.push(name.to_string());
        repos.sort();
        fs::write(&repos_path, serde_json::to_string_pretty(&repos)?)?;
    }
    Ok(())
}

async fn run_process(args: ProcessArgs) -> Result<()> {

    #[cfg(feature = "profiling")]
    let _guard = pprof::ProfilerGuardBuilder::default()
        .frequency(1000)
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()
        .expect("failed to start profiler");

    let extensions: Option<Vec<String>> = args.extensions.map(|e| {
        e.split(',').map(|s| s.trim().to_string()).collect()
    });
    let include_set = build_glob_set(args.include)?;
    let exclude_set = build_glob_set(args.exclude)?;

    fs::create_dir_all(&args.output_dir)?;

    let cache_dir = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("strata")
        .join("blame-cache");
    fs::create_dir_all(&cache_dir)?;
    debug!("Blame cache: {}", cache_dir.display());
    let cache = sled::Config::new()
        .path(&cache_dir)
        // Flush more eagerly during writes (default is 1 GiB — causes a multi-minute
        // stall on exit for large repos as the whole buffer drains at once).
        .cache_capacity(128 * 1024 * 1024)
        .open()
        .context("Failed to open sled blame cache")?;

    let repo_path = ensure_repo(&args.repo, args.ssh_key)?;
    let name = repo_name(&args.repo);

    let ignore_revs_file = {
        let candidate = repo_path.join(".git-blame-ignore-revs");
        if candidate.is_file() {
            info!("using .git-blame-ignore-revs at {}", candidate.display());
            Some(candidate)
        } else {
            None
        }
    };

    info!("Walking commit history");
    let all_commits = get_commit_list(&repo_path)?;
    let sampled = sample_commits(all_commits, args.samples);
    info!("{} commits sampled", sampled.len());

    info!("Collecting work items across {} commits", sampled.len());
    // collect_work_items returns compact (commit_oid, blob_oid) pairs plus a
    // deduplicated blame_lookup: one (commit_oid, path) per unique blob.
    // Keeping file_path out of every WorkItem avoids O(commits × files) String allocations.
    let (items, blame_lookup) = collect_work_items(&repo_path, &sampled, &extensions, &include_set, &exclude_set)?;
    let unique_count = blame_lookup.len();
    let dedup_pct = (1.0 - unique_count as f64 / items.len().max(1) as f64) * 100.0;
    info!(
        "{} work items → {} unique blobs ({dedup_pct:.0}% deduplicated)",
        items.len(), unique_count
    );

    let pb = ProgressBar::new(unique_count as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  {spinner:.cyan} blaming  [{bar:40.cyan/237}] {pos}/{len}  eta {eta}")?
            .progress_chars("━━╾─"),
    );

    let yearly = args.granularity == Granularity::Year;
    let blame_pool = build_blame_pool(args.jobs)?;

    // Blame each unique blob and aggregate to histograms inline — raw lines are
    // dropped as each closure returns, so we never hold all blame output in memory.
    let (blob_hists, hits) = run_blame_phase(
        &blame_lookup, &repo_path, args.no_cache, &cache, &blame_pool, &pb, yearly,
        ignore_revs_file.as_deref(),
    );

    pb.finish_and_clear();
    info!(
        "Blame complete  ({hits}/{unique_count} cache hits, {:.0}%)",
        hits as f64 / unique_count.max(1) as f64 * 100.0
    );
    drop(blame_lookup);

    // Fan out: for each work item, add the blob's histogram counts: O(items × periods)
    // Keying by OID (not ts) prevents same-second commits from inflating a single bucket.
    let (agg, author_agg) = aggregate(&items, &blob_hists);
    drop(items);
    drop(blob_hists);

    let mut all_period_ints: Vec<i32> = agg
        .keys()
        .map(|(_, p)| *p)
        .collect::<FxHashSet<_>>()
        .into_iter()
        .collect();
    all_period_ints.sort();
    let all_periods: Vec<String> = all_period_ints
        .iter()
        .map(|&p| period_int_to_string(p, yearly))
        .collect();
    debug!(
        "{} distinct periods: {} … {}",
        all_periods.len(),
        all_periods.first().unwrap_or(&String::new()),
        all_periods.last().unwrap_or(&String::new())
    );

    let (active_authors, has_other, commit_other) =
        select_authors(&author_agg, &sampled, args.author_threshold);
    debug!("{} active authors (threshold {:.0}%)", active_authors.len(), args.author_threshold * 100.0);

    let all_authors: Vec<String> = {
        let mut v = active_authors.clone();
        if has_other { v.push("other".to_string()); }
        v
    };

    let repo_for_meta = Repository::open(&repo_path)?;
    let series = build_series(
        &sampled, &agg, &author_agg, &active_authors, has_other,
        &commit_other, &all_period_ints, &repo_for_meta,
    );

    let tags = get_version_tags(&repo_path).unwrap_or_default();

    let head_commit = repo_for_meta
        .head()
        .ok()
        .and_then(|r| r.peel_to_commit().ok())
        .map(|c| c.id().to_string())
        .unwrap_or_default();

    let total_lines: u64 = series.last().map(|s| s.total).unwrap_or(0);
    info!("Latest snapshot: {total_lines} total lines across {} periods", all_periods.len());

    let output = OutputData {
        repo: name.clone(),
        granularity: args.granularity.to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        head_commit,
        periods: all_periods,
        authors: all_authors,
        series,
        tags,
    };

    write_output(&output, &args.output_dir, &name)?;

    #[cfg(feature = "profiling")]
    if let Ok(report) = _guard.report().build() {
        use std::io::Write;

        let file = std::fs::File::create("flamegraph.svg")?;
        report.flamegraph(file)?;

        let mut folded = std::io::BufWriter::new(std::fs::File::create("profile.folded")?);
        let mut tally: FxHashMap<String, isize> = FxHashMap::default();
        for (frames, count) in &report.data {
            let stack: Vec<String> = frames.frames.iter().flatten().rev()
                .map(|sym| {
                    sym.name.as_deref()
                        .and_then(|b| std::str::from_utf8(b).ok())
                        .unwrap_or("<unknown>")
                        .to_string()
                })
                .collect();
            writeln!(folded, "{} {count}", stack.join(";"))?;
            for name in &stack {
                *tally.entry(name.clone()).or_insert(0isize) += count;
            }
        }
        drop(folded);

        let mut ranked: Vec<_> = tally.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1));
        let mut summary = std::io::BufWriter::new(std::fs::File::create("profile_summary.txt")?);
        writeln!(summary, "{:>8}  function", "samples")?;
        writeln!(summary, "{}", "-".repeat(72))?;
        for (name, count) in ranked.iter().take(20) {
            writeln!(summary, "{count:>8}  {name}")?;
        }

        eprintln!("profiling output:");
        eprintln!("  flamegraph.svg       — open in browser");
        eprintln!("  profile_summary.txt  — top-20 hottest functions");
        eprintln!("  profile.folded       — collapsed stacks for further analysis");
    }

    Ok(())
}
