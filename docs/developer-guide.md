# Developer Guide

## Table of contents

- [Repository layout](#repository-layout)
- [Architecture overview](#architecture-overview)
- [Data flow](#data-flow)
- [Module reference](#module-reference)
- [Key design decisions](#key-design-decisions)
- [Output format](#output-format)
- [Dependencies](#dependencies)
- [Building and testing](#building-and-testing)
- [Profiling](#profiling)
- [Contributing](#contributing)

---

## Repository layout

```
strata/
├── src/
│   ├── main.rs      — CLI, orchestration, aggregation, serialisation
│   ├── blame.rs     — git blame subprocess: spawn + parse
│   ├── repo.rs      — clone/open repo, commit walking, work-item collection
│   ├── ssh.rs       — SSH credential resolution chain
│   ├── period.rs    — timestamp ↔ quarter/year key; version tag extraction
│   └── types.rs     — serde types: OutputData, SeriesPoint, Tag, WorkItem
├── web/
│   ├── index.html   — shell; no build step
│   ├── app.js       — ES module; chart rendering, data decoding, UI
│   └── styles.css   — dark/light theme via data-theme on <html>
├── data/            — generated output (gitignored)
└── Cargo.toml
```

---

## Architecture overview

```
              ┌─────────────────────────────────────────────────┐
              │                   main.rs                        │
              │                                                  │
  -r URL ────►│ ensure_repo()  ──── repo.rs (clone / open)      │
              │                                                  │
              │ get_commit_list()  ─ git2 revwalk                │
              │ sample_commits()   ─ evenly-spaced N commits     │
              │ collect_work_items() ─ (commit, file, blob) tuples│
              │                                                  │
              │ blob deduplication  ─ FxHashMap<blob_oid, items> │
              │                                                  │
              │ rayon (--jobs threads, default 8)                │
              │   for each unique blob:                          │
              │     sled cache hit?  ─────────────────► lines   │
              │     spawn_blame()   ── blame.rs                  │
              │       git blame --porcelain                      │
              │     parse_blame_output() ── stores in sled       │
              │                                                  │
              │ aggregate:                                        │
              │   per-blob period histograms (par)               │
              │   fan-out to per-commit aggregates               │
              │   author bucketing (threshold)                   │
              │                                                  │
              │ get_version_tags() ── period.rs                  │
              │                                                  │
              │ serialise ─── rmp-serde ──► data/<name>.msgpack  │
              │             serde_json  ──► data/repos.json      │
              └─────────────────────────────────────────────────┘

              web/app.js
                fetch data/<name>.msgpack
                msgpack decode → OutputData
                canvas 2D chart (d3-scale-chromatic colours)
```

---

## Data flow

1. **Clone / open** (`repo.rs`) — If the input is a local path that's a valid git repo, use it directly. Otherwise clone to `$TMPDIR/strata/downloads/<name>-<hash>/`. HTTPS repos shell out to `git clone`; SSH repos use libgit2 with the credential chain in `ssh.rs`.

2. **Commit list** (`repo.rs`) — Walk the full commit graph with libgit2 (time-sorted). Fall back to pushing all branch tips if HEAD is unresolvable.

3. **Sampling** (`repo.rs`) — Select `N` commits at evenly-spaced indices across the sorted list. This gives uniform temporal coverage rather than clustering around busy periods.

4. **Work items** (`repo.rs`) — For each sampled commit, walk the tree and emit a `WorkItem` (commit OID, file path, blob OID) for every file that matches the extension filter.

5. **Blob deduplication** (`main.rs`) — Group work items by blob OID. If the same file content appears in 50 sampled commits, it only needs one blame call. The dedup ratio is logged at `-v`.

6. **Parallel blame** (`blame.rs`) — A dedicated Rayon pool (size controlled by `--jobs`, default 8 — see [why](#parallel-blame-thread-pool)) runs blame for each unique blob. Each thread checks the sled cache first; on a miss it spawns `git blame --porcelain -w` (plus `--ignore-revs-file` if `.git-blame-ignore-revs` exists in the repo root) and stores the result.

7. **Aggregation** (`main.rs`) — Two passes:
   - **Per-blob histogram**: for each blob, count lines per period key and per author. Run in parallel.
   - **Fan-out**: for each work item, add the blob's histogram into the commit-level aggregates. This is O(items × periods) but periods is small (tens to low hundreds).

8. **Author bucketing** (`main.rs`) — At the HEAD commit, sort authors by line count descending, accumulate until ≥ threshold coverage, mark the rest as "other".

9. **Serialisation** (`main.rs`) — Build `OutputData`, encode with `rmp-serde`, write to `data/<name>.msgpack`. Update `data/repos.json`.

10. **Frontend rendering** (`web/app.js`) — `@msgpack/msgpack` decodes the binary file. A canvas 2D chart renders sampled commits on the x-axis, line counts as stacked bars, coloured by `d3-scale-chromatic` interpolators.

---

## Module reference

### `main.rs`

Entry point and orchestration. Owns:
- CLI argument parsing via `clap` derive
- Tracing/logging initialisation
- The two-phase aggregation (per-blob histograms → per-commit fan-out)
- Author bucketing logic
- `SeriesPoint` construction and `OutputData` serialisation
- Optional profiling output (behind `--features profiling`)

### `blame.rs`

Two public functions:

**`spawn_blame(repo_path, commit_oid, file_path, ignore_revs)`** — Spawns `git blame --porcelain -w <commit> -- <file>` as a subprocess with `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`, and `GIT_OPTIONAL_LOCKS=0` to eliminate per-process config-file overhead across thousands of concurrent invocations. `-w` is always passed so whitespace-only changes don't shift attribution. If `ignore_revs` is `Some(path)`, `--ignore-revs-file=<path>` is appended to skip bulk commits (e.g. formatting runs) from attribution.

**`parse_blame_output(stdout, blob_oid, cache)`** — Parses the porcelain format line by line. Commit metadata (author name, author-time) appears once per commit in the output; subsequent lines for the same commit reference the cached values. Emits one `(timestamp, author)` pair per source line. Stores the result in sled keyed by blob OID before returning.

### `repo.rs`

**`ensure_repo(url, ssh_key)`** — Handles the local/remote/cached-clone decision tree.

**`get_commit_list(repo_path)`** — libgit2 revwalk, time-sorted, with HEAD-unborn fallback.

**`sample_commits(commits, n)`** — Evenly-spaced index selection: `commits[i * (len-1) / (n-1)]` for `i` in `0..n`. Preserves both the oldest and newest commits.

**`collect_work_items(repo_path, sampled, extensions)`** — Tree walk via libgit2 for each sampled commit, emitting one `WorkItem` per blob entry that passes the extension filter.

### `ssh.rs`

**`make_auth_callbacks(remote_url, explicit_key)`** — Builds a libgit2 `RemoteCallbacks` with a credentials closure that cycles through:
1. Explicit key (if `--key` was passed)
2. SSH agent (tried once)
3. `IdentityFile` entries from `~/.ssh/config` matching the target hostname (with SSH glob support: `*`, `?`, case-insensitive)
4. All `id_*` files in `~/.ssh/` (sorted, `.pub` excluded)

Uses atomic flags/indices to handle libgit2's retry-on-failure callback model without infinite loops.

### `period.rs`

**`ts_to_period_int(ts, yearly)`** — Maps a Unix timestamp to a compact `i32` key:
- Quarter mode: `year * 4 + quarter_index` (0-based)
- Year mode: `year * 4` (quarter always 0, so keys sort correctly)
- Returns `i32::MIN` for unparseable timestamps

**`period_int_to_string(p, yearly)`** — Inverse: `"2019-Q3"` or `"2019"`.

**`get_version_tags(repo_path)`** — Finds all semver tags (`v?MAJOR.MINOR.PATCH`), peels them to commits, and computes an `importance` score for each tag based on the minimum gap to its nearest neighbour. Tags isolated by a large gap in time get a high importance score; closely-spaced tags (e.g. patch releases) get a low score. The frontend uses importance to decide label prominence.

### `types.rs`

```rust
pub struct OutputData {
    pub repo: String,          // derived from the URL/path
    pub granularity: String,   // "quarter" | "year"
    pub generated_at: String,  // RFC3339 timestamp
    pub periods: Vec<String>,  // e.g. ["2018-Q1", "2018-Q2", …]
    pub authors: Vec<String>,  // top authors + optionally "other"
    pub series: Vec<SeriesPoint>,
    pub tags: Vec<Tag>,
}

pub struct SeriesPoint {
    pub ts: i64,                        // Unix timestamp of the sampled commit
    pub total: u64,                     // total lines in the snapshot
    pub counts: Vec<(usize, u64)>,      // (period_index, line_count) — sparse
    pub author_counts: Vec<(usize, u64)>, // (author_index, line_count) — sparse
    pub summary: String,                // commit message summary
    pub author: String,                 // commit author name
}

pub struct Tag {
    pub name: String,    // e.g. "v1.2.3"
    pub ts: i64,         // Unix timestamp of the tagged commit
    pub importance: f64, // 0.0–1.0; higher = more isolated in time
}
```

`counts` and `author_counts` are sparse: only non-zero entries are stored. The frontend reconstructs the full array by index lookup.

---

## Key design decisions

### Shell out for blame, not libgit2

libgit2's `blame_file()` is roughly 100× slower than the system `git blame` binary for the same file. strata shells out to `git` for all blame work. The subprocess environment is stripped to the minimum (`GIT_CONFIG_NOSYSTEM`, `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_OPTIONAL_LOCKS=0`) to shave config-reading overhead across thousands of concurrent invocations.

`-w` is always passed so whitespace-only reformatting commits don't shift authorship credit. If the repo root contains a `.git-blame-ignore-revs` file, `--ignore-revs-file` is passed automatically — `GIT_CONFIG_GLOBAL=/dev/null` would otherwise suppress any config-based `blame.ignoreRevsFile`, so strata detects the file itself and injects the flag directly.

libgit2 is still used for everything else — commit walking, tree walking, SSH credential handling — where it's fast and convenient.

### Parallel blame thread pool

The blame phase is subprocess I/O-bound, not CPU-bound. Each Rayon thread spawns a `git blame` process and blocks on its output. With a standard CPU-count pool, most threads would be idle waiting on I/O. A dedicated pool is used (rather than the global Rayon pool) to avoid interfering with CPU-bound parallel work elsewhere.

The pool size defaults to 8 (a safe, low-impact value) and is configurable via `--jobs`. Users on fast local storage who want maximum throughput can raise it to 64–128 or higher. The previous hardcoded value was 512.

### Blob-level deduplication

A file's content (its blob OID) is identical across any number of commits as long as the file wasn't changed. Blaming the same content twice produces the same result. strata deduplicates work items by blob OID before dispatching blame, so a file unchanged across 50 sampled commits costs one blame call, not 50. The dedup ratio is typically 30–70% on stable codebases.

### Sled blame cache

Blame results are stored in a [sled](https://github.com/spacejam/sled) embedded database keyed by blob OID (the SHA of the file content). This cache is:
- **Persistent across runs** — re-running strata on the same repo is much faster the second time
- **Cross-repo** — the same blob OID in two different repos (e.g. a vendored dependency) hits the cache
- **Content-addressed** — renaming or moving a file doesn't invalidate its entry

Cache entries are serialised with [bincode](https://github.com/bincode-org/bincode) as `Vec<(i64, String)>` (timestamp, author name).

> **Upgrade note:** `-w` changes which commit git attributes each line to, so cache entries produced by older versions of strata (without `-w`) will return stale results. Run with `--no-cache` once after upgrading to force a clean recompute.

### Two-pass aggregation

Aggregating blame results directly per commit would require merging `O(items)` maps serially. Instead:

1. **Per-blob histograms** (parallel): build a period-count and author-count map for each unique blob. This is `O(total_lines)` and fully parallel.
2. **Fan-out** (serial): for each work item, add its blob's histogram into the commit-level aggregate. This is `O(items × periods)` — items can be large but periods is small (tens to low hundreds).

### Author bucketing

Individually tracking every contributor in the chart produces visual noise and a legend that's too long to read. strata computes the smallest set of top authors (by line count at HEAD) that covers ≥ `--author-threshold` of the codebase, and collapses the rest into a single "other" bucket.

---

## Output format

The `.msgpack` file is a MessagePack-encoded `OutputData` struct serialised with field names (`rmp_serde::to_vec_named`). The frontend decodes it with `@msgpack/msgpack`.

### `repos.json`

A JSON array of repo name strings, sorted alphabetically. Updated atomically after each run. Example:

```json
["my-app", "my-lib", "old-monolith"]
```

### `.msgpack` schema

```
OutputData
├── repo: str
├── granularity: str                  "quarter" | "year"
├── generated_at: str                 RFC3339
├── periods: [str, ...]               ["2018-Q1", "2018-Q2", …]
├── authors: [str, ...]               top authors + optional "other"
├── tags: [{name, ts, importance}, …]
└── series: [SeriesPoint, …]
    └── SeriesPoint
        ├── ts: int                   Unix timestamp
        ├── total: int                total lines in snapshot
        ├── counts: [(int, int), …]   sparse (period_idx, count)
        ├── author_counts: [(int, int), …]  sparse (author_idx, count)
        ├── summary: str              commit message summary
        └── author: str               commit author name
```

---

## Dependencies

| Crate | Purpose |
|-------|---------|
| `clap` (derive) | CLI argument parsing |
| `git2` (vendored-libgit2, ssh) | Commit walking, tree walking, SSH cloning |
| `rayon` | Parallel blame dispatch |
| `sled` | Persistent blame cache |
| `rmp-serde` | MessagePack serialisation |
| `serde` + `serde_json` | Struct serialisation; `repos.json` |
| `bincode` | Cache entry serialisation (sled values) |
| `rustc-hash` | Fast `FxHashMap` / `FxHashSet` for hot paths |
| `chrono` | Timestamp → quarter/year conversion |
| `regex` | Semver tag matching |
| `dirs` | Platform cache directory (`~/.cache/`) |
| `indicatif` | Progress bar |
| `tracing` + `tracing-subscriber` | Structured logging |
| `anyhow` | Error propagation |
| `pprof` (optional) | CPU profiling; enabled by `--features profiling` |

The frontend pulls two CDN ES modules at runtime (no build step):
- `@msgpack/msgpack@3` — MessagePack decoder
- `d3-scale-chromatic@3` — colour interpolators and categorical palettes

---

## Building and testing

```sh
# Debug build
cargo build

# Release build (significantly faster blame throughput due to optimised parsing)
cargo build --release

# Check without producing a binary
cargo check

# Lint
cargo clippy

# Format
cargo fmt

# Run tests (none yet — contributions welcome)
cargo test
```

There is currently no test suite. Good places to start:
- Unit tests for `parse_blame_output` in `blame.rs` using synthetic porcelain output
- Unit tests for `ts_to_period_int` / `period_int_to_string` in `period.rs`
- Unit tests for the SSH config parser in `ssh.rs`

---

## Profiling

Build with the `profiling` feature to wrap the run in `pprof`:

```sh
cargo build --release --features profiling
./target/release/strata -r /path/to/repo -s 50
```

Output is written to the current directory on exit:

| File | Use |
|------|-----|
| `flamegraph.svg` | Open in browser; shows cumulative time by call stack |
| `profile_summary.txt` | Quick top-20 hottest functions |
| `profile.folded` | Collapsed stack format; analyse with `inferno-flamegraph` or `speedscope` |

The profiler samples at 1000 Hz and excludes `libc`, `libgcc`, `pthread`, and `vdso` frames to keep the output focused on strata's own code.

---

## Contributing

The project has no formal contribution process. Key areas worth improving:

- **Test coverage** — see [Building and testing](#building-and-testing) for suggested starting points
- **HTTPS authentication** — currently strata shells out to `git clone` for HTTPS, relying on system credential helpers. A pure-libgit2 path with credential-helper support would be cleaner.
- **Incremental updates** — re-running strata on a repo that already has output could skip commits already in the cache. The sled cache already stores blame by blob, but the commit-level aggregation is recomputed from scratch each time.
- **Frontend interactivity** — the web UI renders but doesn't support filtering by author, clicking to open a commit in the browser, or exporting data.
- **Windows support** — the blame subprocess sets `GIT_CONFIG_GLOBAL=/dev/null` (Unix path). The code has a `cfg!(windows)` guard that switches to `NUL`, but this path has not been tested.
