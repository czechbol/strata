# strata

Git code archaeology — fast parallel blame aggregator with an interactive web frontend.

strata walks a repository's commit history, samples commits at even intervals, and runs `git blame` across every file in parallel. It aggregates line-level authorship and age data by quarter or year and writes the result as MessagePack. A zero-build static web frontend reads that file and renders an interactive stacked area chart showing how code evolved over time — by when lines were written and by who wrote them.

<img width="1879" height="936" alt="image" src="https://github.com/user-attachments/assets/b1f5747d-a03e-44ee-b6ba-d378fc5081bb" />

> [!NOTE]
> strata was nearly entirely written by an LLM, since I have zero Rust experience. I did however do a lot of steering, profiling, etc. to make sure it went the right direction.

## Requirements

- Rust (stable, 1.70+)
- `git` on `$PATH` (used for blame and HTTPS clones)

## Build

```sh
cargo build --release
```

The binary lands at `target/release/strata`.

## Quick start

> [!NOTE]
> strata defaults to 8 parallel blame processes (`-j 8`) to be as fast as possible without a greater risk of crashing systems running on modern hardware (with enough CPU threads). Raise it (e.g. `-j 64`) for faster analysis on machines with more threads. Even the default `-j 8` will keep your CPU very busy — tone it down if needed.

```sh
# Analyse a local repo — defaults: 100 sampled commits, all file types, quarterly granularity
./target/release/strata process --repo /path/to/repo

# Faster analysis on a machine with fast storage
./target/release/strata process --repo /path/to/repo -j 64

# Analyse only Rust and TypeScript files
./target/release/strata process --repo /path/to/repo -e .rs,.ts

# Analyse a remote repo via SSH — 50 sampled commits instead of the default 100
./target/release/strata process --repo git@github.com:org/repo.git -s 50

# Analyse all commits (no sampling) — accurate but slow on large repos
./target/release/strata process --repo https://github.com/org/repo.git -s 0

# Serve the results (opens http://localhost:8080)
./target/release/strata serve
```

**Key defaults to know:**

| Flag | Default | Notes |
|------|---------|-------|
| `-s` / `--samples` | `100` | Commits sampled evenly across history; `0` = all commits |
| `-j` / `--jobs` | `8` | Parallel blame processes; raise to go faster |
| `-g` / `--granularity` | `quarter` | Time buckets: `quarter` or `year` |
| `-o` / `--output-dir` | `web/data` | Where `.msgpack` and `repos.json` are written |
| `--author-threshold` | `0.80` | Top authors covering ≥80% of lines are shown individually; the rest become "other" |

## Output

strata writes to `web/data/` by default (override with `-o`):

| File | Description |
|------|-------------|
| `web/data/<repo-name>.msgpack` | MessagePack-encoded analysis data consumed by the web frontend |
| `web/data/repos.json` | Sorted list of repos known to the frontend; updated automatically |

## Web UI

The `strata serve` command starts an HTTP server on port 8080 that serves the embedded web frontend and reads data files from `web/data/` on the filesystem. Open `http://localhost:8080` in your browser.

- **Repo selector** — switch between all repos in `data/repos.json`
- **By period** view — stacked area chart coloured by when lines were written (oldest = dark, newest = bright)
- **By author** view — same chart, coloured by author
- **Scroll to zoom, drag to pan** — explore the full commit timeline
- **Settings gear** — change the colour palette and light/dark theme
- **Hover** — tooltip shows commit message, author, date, and line count breakdown

```sh
# Default: serve web/data/ on port 8080
./target/release/strata serve

# Custom port and data directory
./target/release/strata serve --port 9000 --dir /path/to/data
```

## Documentation

- [User guide](docs/user-guide.md) — all CLI subcommands and flags, SSH authentication, author bucketing, cache behaviour, web UI walkthrough, and common workflows
- [Developer guide](docs/developer-guide.md) — architecture, module breakdown, key design decisions, output format, profiling, and contributing

## Acknowledgements

Inspired by [gitcharts](https://github.com/koaning/gitcharts) by Vincent D. Warmerdam — the original idea and Python/marimo implementation ([talk](https://www.youtube.com/watch?v=BxIsPxBAxHQ)).
