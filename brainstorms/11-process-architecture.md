# 11 — Process architecture: crawler and UI are separate processes

**Decision:** two binaries in one Cargo workspace, talking only through the SQLite database. **Local-only deployment**: the UI binds `127.0.0.1` and has no auth.

## Workspace layout
```
Cargo.toml              # [workspace]
crates/
  core/                 # shared: models, DB schema + migrations, queries, event types, config, URL utils
  llm/                  # provider trait + OpenAI-compatible client, prompts, cache
  crawler/              # bin: frontier, fetcher, browser, classifier, extractor, metrics sampler
  ui/                   # bin: axum server, NL search, static frontend
```
(Packages are named `career-core`, `career-llm`, `career-crawler` and `career-ui`, because `core` would shadow Rust's `core` crate.)

`core` owns the schema and migrations. Both binaries call `core::db::open()`, which runs migrations, so it doesn't matter which process starts first.

## Why this works with SQLite
- **WAL mode** lets the crawler write while the UI reads at the same time.
- The crawler is the **only writer** of crawl data (through its single writer task). The UI writes only to its own tables (`control_commands`, and `llm_calls` for NL queries).
- Set `busy_timeout` in both processes.

## Live events across processes
`tokio::sync::broadcast` can't cross process boundaries. Options:
1. **UI tails the `events` table** (recommended): poll `SELECT … WHERE id > ?last ORDER BY id LIMIT 500` every ~200 ms, then fan out to browser clients over WS/SSE through a broadcast channel inside the UI process.
   - Upsides: one code path for history and live, it works when the crawler is down or restarting, and nothing extra runs.
   - Downside: ~200 ms latency, which is fine for this.
2. The crawler exposes a local WS/Unix socket that the UI subscribes to. Lower latency, but it couples the two processes and duplicates the event path. Not worth it now.

Chatty events (`url_enqueued`) are aggregated by the crawler before they're written (e.g. one `frontier_batch` event per second), so the events table doesn't explode.

## UI → crawler control
Pause, resume, stop, change budgets, add seeds, force a re-crawl of a domain:
- The UI inserts rows into `control_commands(id, ts, command, args_json, status)`.
- The crawler polls that table about once a second, applies each command, sets `status = 'done'`, and emits an event.

**Implemented (milestone 7):**
- The crawler polls `control_commands` every 500 ms (`crawler/src/control.rs`), applies `pause` / `resume` / `stop`, marks each row `done`, and appends a `control_applied` event. Unknown commands get marked `unknown`.
- Commands still `pending` when a crawler starts are marked `expired`, so a stale stop can't end a new run.
- While paused the scheduler dispatches nothing new, and in-flight visits finish.
- On `stop` the run ends with reason `stopped` (Ctrl-C: `interrupted`). In-flight visits finish, but optional sitemap scans are skipped.
- Verified with both processes live: the page count held steady while paused, then resumed and stopped from the UI.

## Running
```bash
cargo run -p career-crawler -- --config config.toml
cargo run -p career-ui -- --db data/career.db   # http://127.0.0.1:7878
```
The UI is fully usable on its own for browsing history and running NL search while the crawler isn't running.
