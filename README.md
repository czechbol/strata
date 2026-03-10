# strata

Git code archaeology — fast parallel blame aggregator with an interactive web frontend.

strata walks a repository's commit history, samples commits at even intervals, and runs `git blame` across every file in parallel. It aggregates line-level authorship and age data by quarter or year and writes the result as MessagePack. A zero-build static web frontend reads that file and renders an interactive stacked area chart showing how code evolved over time — by when lines were written and by who wrote them.

<img width="1879" height="936" alt="image" src="https://github.com/user-attachments/assets/b1f5747d-a03e-44ee-b6ba-d378fc5081bb" />

> [!NOTE]
> strata was nearly entirely written by an LLM, since I have zero Rust experience. I did however do a lot of steering, profiling, etc. to make sure it went the right direction.

## Requirements

- Rust (stable, 1.70+)
- `git` on `$PATH` (used for blame and HTTPS clones)
- Any static HTTP server for the frontend (`npx serve`, `python3 -m http.server`, etc.)

## Build

```sh
cargo build --release
```

The binary lands at `target/release/strata`.

## Quick start

> [!NOTE]
> strata defaults to 8 parallel blame processes (`-j 8`) to be as fast as possible without a greater risk of crashing systems running on modern hardware (with enough CPU threads). Raise it (e.g. `-j 64`) for faster analysis on machines with more threads. Even the default `-j 8` will keep your CPU very busy — tone it down if needed.

```sh
# Analyse a local repo (100 sampled commits, all file types)
./target/release/strata -r /path/to/repo

# Faster analysis on a machine with fast storage
./target/release/strata -r /path/to/repo -j 64

# Analyse only Rust and TypeScript files
./target/release/strata -r /path/to/repo -e .rs,.ts

# Analyse a remote repo via SSH (50 sampled commits)
./target/release/strata -r git@github.com:org/repo.git -s 50

# Analyse a remote repo via HTTPS
./target/release/strata -r https://github.com/org/repo.git -s 50

# Serve the results
npx serve .
# or: python3 -m http.server
# then open http://localhost:3000 (or whatever port)
```

## Output

strata writes to the `data/` directory (override with `-o`):

| File | Description |
|------|-------------|
| `data/<repo-name>.msgpack` | MessagePack-encoded analysis data consumed by the web frontend |
| `data/repos.json` | Sorted list of repos known to the frontend; updated automatically |

## Web UI

The frontend lives in `web/` and is a single static page with no build step. Open it through a local HTTP server (direct `file://` access won't work — the page fetches files via `fetch()`).

- **Repo selector** — switch between all repos in `data/repos.json`
- **By period** view — stacked area chart coloured by when lines were written (oldest = dark, newest = bright)
- **By author** view — same chart, coloured by author
- **Scroll to zoom, drag to pan** — explore the full commit timeline
- **Settings gear** — change the colour palette and light/dark theme
- **Hover** — tooltip shows commit message, author, date, and line count breakdown

## Documentation

- [User guide](docs/user-guide.md) — all CLI flags, SSH authentication, author bucketing, cache behaviour, web UI walkthrough, and common workflows
- [Developer guide](docs/developer-guide.md) — architecture, module breakdown, key design decisions, output format, profiling, and contributing

## Acknowledgements

Inspired by [gitcharts](https://github.com/koaning/gitcharts) by Vincent D. Warmerdam — the original idea and Python/marimo implementation ([talk](https://www.youtube.com/watch?v=BxIsPxBAxHQ)). strata is an independent Rust reimplementation, though the Python version could likely be optimised to similar speeds — the bottleneck is git I/O, not the language. For me this was a fun opportunity to try vibecoding in a language I'm not familiar with and try to optimize the hell out of it.
