# User Guide

strata is a CLI tool for git code archaeology. It samples a repository's commit history, runs `git blame` in parallel across every file, and aggregates the results into an interactive chart showing how code age and authorship have evolved over time.

## Table of contents

- [Prerequisites](#prerequisites)
- [CLI flags](#cli-flags)
- [Choosing what to analyse](#choosing-what-to-analyse)
- [SSH authentication](#ssh-authentication)
- [Author bucketing](#author-bucketing)
- [Blame cache](#blame-cache)
- [Granularity](#granularity)
- [Output directory](#output-directory)
- [Logging](#logging)
- [Profiling build](#profiling-build)
- [Serving the web UI](#serving-the-web-ui)
- [Web UI walkthrough](#web-ui-walkthrough)
- [Common workflows](#common-workflows)

---

## Prerequisites

- **Rust** (stable) for building
- **`git`** on `$PATH` — used for running blame and for cloning HTTPS repos
- **A local HTTP server** for the frontend (any will do)

---

## CLI flags

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--repo` | `-r` | required | Repository URL or local path |
| `--samples` | `-s` | `100` | Number of commits to sample; `0` samples every commit |
| `--extensions` | `-e` | (all files) | Comma-separated file extensions to include |
| `--granularity` | `-g` | `quarter` | Time resolution: `quarter` or `year` |
| `--output-dir` | `-o` | `data` | Directory where output files are written |
| `--key` | `-k` | (auto) | Path to an SSH private key |
| `--jobs` | `-j` | `8` | Max parallel blame processes; raise to go faster at the cost of CPU/IO |
| `--no-cache` | | false | Ignore cached blame results and recompute |
| `--author-threshold` | | `0.80` | Fraction of lines top authors must cover (0.0–1.0) |
| `--verbose` / `-v` | | (warn) | Log verbosity: `-v` info, `-vv` debug, `-vvv` trace |

---

## Choosing what to analyse

### Repository source

Pass any of the following to `-r`:

```sh
# Local directory
strata -r /path/to/repo

# SSH URL (SCP-style)
strata -r git@github.com:org/repo.git

# SSH URL (explicit scheme)
strata -r ssh://git@github.com/org/repo.git

# HTTPS URL
strata -r https://github.com/org/repo.git
```

Remote repos are cloned into a temporary directory (`$TMPDIR/strata/downloads/<name>-<hash>/`). If a valid clone already exists there, strata reuses it without re-cloning.

### Commit sampling

strata doesn't analyse every commit by default — that would be prohibitively slow on large repositories. Instead it selects `N` commits spaced evenly across the full commit timeline:

```sh
# 100 commits (default) — good for most repos
strata -r /path/to/repo -s 100

# 50 commits — faster, less temporal resolution
strata -r /path/to/repo -s 50

# All commits — accurate but slow on large repos
strata -r /path/to/repo -s 0
```

More samples = smoother chart and finer temporal resolution, but proportionally more blame work. For a repo with 10 000 commits, `-s 100` means one commit every 100.

### Filtering by extension

Without `-e`, strata blames every file in the tree. Filter to specific languages to reduce noise and speed up analysis:

```sh
# Rust only
strata -r /path/to/repo -e .rs

# Rust, TypeScript, and JavaScript
strata -r /path/to/repo -e .rs,.ts,.js

# Python only
strata -r /path/to/repo -e .py
```

Extensions are matched case-sensitively. Include the leading dot.

---

## SSH authentication

When cloning a repository via SSH, strata tries credentials in this order:

1. **Explicit key** — if you passed `--key <path>`, only that key is tried.
2. **SSH agent** — tries the agent socket if one is running.
3. **`~/.ssh/config` IdentityFile** — reads your SSH config for `Host` blocks matching the target hostname (respects glob patterns like `*.github.com`).
4. **`~/.ssh/id_*` discovery** — falls back to all private key files (`id_rsa`, `id_ed25519`, etc.) in your `~/.ssh/` directory, sorted alphabetically.

If all options are exhausted, strata exits with an authentication error and a hint about what to try.

```sh
# Override auth with a specific key
strata -r git@github.com:org/repo.git --key ~/.ssh/deploy_key
```

For HTTPS repos, strata shells out to `git clone`, so your system Git credential helpers (keychain, `git-credential-manager`, etc.) apply automatically.

---

## Author bucketing

Real codebases often have dozens or hundreds of contributors. Plotting them all individually makes the chart unreadable. strata solves this by computing the smallest set of authors whose combined lines at HEAD cover at least `--author-threshold` (default 80%) of the total, then bucketing everyone else as **"other"**.

```sh
# Default: top authors covering ≥80% of lines at HEAD are shown individually
strata -r /path/to/repo

# Show more authors: lower threshold means fewer lines bucketed as "other"
strata -r /path/to/repo --author-threshold 0.95

# Minimal: only the single biggest contributor is named; everyone else is "other"
strata -r /path/to/repo --author-threshold 0.50
```

The threshold applies at the HEAD snapshot. Authors who wrote a lot historically but have little code remaining at HEAD may still fall into "other".

---

## Blame cache

Blame results are expensive to compute. strata caches them in a [sled](https://github.com/spacejam/sled) embedded database at:

```
~/.cache/strata/blame-cache/
```

Cache entries are keyed by blob OID (the SHA of the file content), so:
- The same file content seen in multiple commits is blamed exactly once, ever.
- Entries survive across runs and across different repositories.
- Moving or renaming a file doesn't invalidate its cache entry as long as the content didn't change.

To force a fresh run (e.g. after suspecting a corrupted entry):

```sh
strata -r /path/to/repo --no-cache
```

You can delete the entire cache directory to reclaim disk space at any time:

```sh
rm -rf ~/.cache/strata/blame-cache/
```

---

## Parallelism

strata runs `git blame` in parallel across all unique file blobs. The `-j` flag controls how many blame processes run concurrently:

```sh
# Default (8) — low system impact, suitable for background use
strata -r /path/to/repo

# More parallelism — faster on machines with spare cores and fast storage
strata -r /path/to/repo -j 32

# Maximum throughput — saturate I/O on a dedicated machine
strata -r /path/to/repo -j 128
```

Since blame is subprocess I/O-bound rather than CPU-bound, you can raise `-j` well above your CPU count without degrading other work. The right value depends on your storage speed: SSDs with parallel read paths benefit from higher values; network filesystems and spinning disks may not.

---

## Granularity

Controls the time resolution of the x-axis buckets:

```sh
# Quarter (default): 2019-Q1, 2019-Q2, …
strata -r /path/to/repo -g quarter

# Year: 2019, 2020, …
strata -r /path/to/repo -g year
```

Year mode is useful when a repository spans many decades or when the chart looks too noisy with quarterly resolution.

---

## Output directory

By default, files are written to `data/` relative to the current working directory. The directory is created if it doesn't exist.

```sh
# Write to a custom directory
strata -r /path/to/repo -o /var/www/strata-data
```

Files written:

| File | Description |
|------|-------------|
| `<repo-name>.msgpack` | Analysis data in MessagePack format |
| `repos.json` | Sorted list of all repo names; used by the web frontend |

---

## Logging

```sh
strata -r /path/to/repo          # only warnings (default)
strata -r /path/to/repo -v       # info: progress milestones, dedup stats, cache hit rate
strata -r /path/to/repo -vv      # debug: per-commit detail, SSH key selection, period ranges
strata -r /path/to/repo -vvv     # trace: per-file blame entries
```

You can also override log level with the `RUST_LOG` environment variable:

```sh
RUST_LOG=strata=debug strata -r /path/to/repo
```

---

## Profiling build

strata ships an optional `profiling` feature that wraps the run in [pprof](https://github.com/tikv/pprof-rs) and writes output on exit:

```sh
cargo build --release --features profiling
./target/release/strata -r /path/to/repo
```

Output files written to the current directory:

| File | Description |
|------|-------------|
| `flamegraph.svg` | Interactive flamegraph; open in any browser |
| `profile_summary.txt` | Top-20 hottest functions by sample count |
| `profile.folded` | Collapsed stacks for further analysis with inferno or similar |

---

## Serving the web UI

The `web/` directory contains the frontend. It requires a local HTTP server — direct `file://` access doesn't work because `app.js` uses `fetch()` to load the data files.

```sh
# From the repository root (serves both web/ and data/)
npx serve .

# Python
python3 -m http.server

# Any other static server
caddy file-server
```

Open the URL printed by the server (usually `http://localhost:3000` or `http://localhost:8000`).

The page loads `data/repos.json` to populate the repo selector, then fetches `data/<repo-name>.msgpack` when a repo is selected.

---

## Web UI walkthrough

### Repo selector

The dropdown in the header lists all repos from `data/repos.json`. Switching repos fetches and decodes the corresponding `.msgpack` file. The chart renders immediately once decoding completes.

### Period vs author views

The **by period** button shows each sampled commit as a stacked bar, with stacks coloured by when lines were written (darker = older, brighter = newer). This answers: "how much of the code at any point in time was recently written vs years old?"

The **by author** button recolours the stacks by who wrote each line. This answers: "how has each contributor's share of the codebase evolved?"

### Navigation

- **Scroll / pinch** to zoom in and out along the time axis
- **Click and drag** to pan
- **Hover** over any commit to see a tooltip with:
  - Commit message and author
  - Total line count
  - Breakdown by period or author (depending on current view)

Version tags (semver only) are shown as vertical lines. Tags with a longer gap to their neighbours are shown with a more prominent label.

### Settings

The gear icon in the top-right opens a panel for:

- **Theme** — auto (follows system), dark, or light
- **Period colour scheme** — Viridis (default), Turbo, Plasma, Inferno, Magma, Cividis, Spectral, Rainbow, Cool, Warm
- **Author colour scheme** — Tableau 10 (default), Category 10, Set 2, Set 3, Dark 2, Paired, Accent

---

## Common workflows

### Analyse multiple repos and compare

Run strata on each repo, all writing to the same `data/` directory:

```sh
strata -r /path/to/repo-a -o data/
strata -r /path/to/repo-b -o data/
npx serve .
```

Both repos appear in the frontend's dropdown.

### Focus on application code only

```sh
strata -r /path/to/repo -e .go,.proto
strata -r /path/to/repo -e .py,.pyi
strata -r /path/to/repo -e .ts,.tsx,.js
```

### Track a long-lived repo at yearly resolution

```sh
strata -r /path/to/old-repo -g year -s 200
```

### Re-run with fresh blame (cache bypass)

```sh
strata -r /path/to/repo --no-cache
```

### Run against a private repo with a specific deploy key

```sh
strata -r git@github.com:myorg/private-repo.git --key ~/.ssh/id_ed25519_deploy
```

### Speed up analysis on a fast machine

```sh
strata -r /path/to/repo -j 64
```

### Keep system impact low (background run)

```sh
strata -r /path/to/repo -j 4
```

### Verbose run to understand what's happening

```sh
strata -r /path/to/repo -vv -s 20
```
