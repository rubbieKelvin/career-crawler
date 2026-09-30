# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

Greenfield Rust crate (`career`, edition 2024). `src/main.rs` is still the cargo hello-world stub. Design notes live in `brainstorms/` (numbered `NN-topic.md`); read `brainstorms/00-overview.md` first — it is the source of truth for intent until real code exists. When a brainstorm decision is implemented or overturned, update the brainstorm rather than letting it drift.

## What this program is

A recursive web crawler whose goal is finding **company career pages and the job postings on them**, storing results in a local SQLite database. It must:

1. Decide *which* sites are worth crawling (prioritized frontier, not a blind BFS).
2. On a site judged reputable/company-like, locate its careers/jobs link and crawl that section.
3. Extract job postings and store them with normalized fields (location, salary, category).
4. Serve a web UI that shows the live crawl and the crawl history as a network graph, updated in real time, plus metrics and natural-language job search (e.g. "nice paying jobs in tech around Lagos").

## Commands

```bash
cargo build
cargo run                      # still the single stub crate; see planned layout below
cargo test                     # all tests
cargo test <name_substring>    # single test / group
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Once the workspace exists (planned): `cargo run -p crawler -- --config config.toml`, `cargo run -p ui -- --db career.db --port 7878`, and `cargo test -p <crate> <name>`.

## Intended architecture

**Two processes, one SQLite DB (WAL), local only.** The crawler and the UI are separate binaries in one Cargo workspace. They never talk directly; everything goes through the DB (`brainstorms/11-process-architecture.md`):
- **crawler → UI**: the crawler appends to the `events` table, and the UI tails it (`WHERE id > last`, ~200 ms) and fans out over WS/SSE. So live view and history replay share one code path, and the UI works while the crawler is stopped.
- **UI → crawler**: the UI inserts into `control_commands`, and the crawler polls it (pause/resume, headless on/off, add seeds).
- The crawler is the only writer of crawl data, through a single writer task. The UI writes only its own tables.

Planned crates:
- **core**: models, schema + migrations (both binaries run them on open), queries, event types, config, URL normalization.
- **llm**: an OpenAI-compatible chat client (DeepSeek first; `base_url`/`model`/key-env are config, so switching providers is a config change), versioned prompts in `prompts/`, and a response cache in `llm_calls`. Output is always JSON parsed into serde structs, falling back to heuristics on failure. Every LLM path must be optional (`llm.enabled = false` still crawls). See `brainstorms/10-llm.md`.
- **crawler**, whose internal pipeline is `frontier → fetcher → parser → (classifier, extractor) → store + events → frontier`:
  - *frontier*: a DB-backed priority queue scored per link; per-host politeness (robots.txt, rate limit); per-domain budgets.
  - *fetcher*: reqwest with auto-decompression **off**, so the metrics count wire bytes.
  - *browser*: headless Chromium (`chromiumoxide`), behind cargo feature `headless` **and** a runtime flag. It's used only for careers pages that yield no jobs or look like SPA shells. It captures jobs-API XHRs so later visits can skip the browser.
  - *classifier*: heuristic domain/link scoring as pure functions (test them offline with fixtures), plus the LLM for gray-zone cases only.
  - *extractor*: careers-link discovery, then jobs in tier order: ATS APIs → JSON-LD `JobPosting` → HTML heuristics → LLM. Enrichment adds the normalized geo/salary/category needed for NL search.
  - *metrics*: `Arc<Metrics>` atomics plus a 1s sampler (CPU/RSS/heap/bytes/LLM tokens) → `metrics_samples` + `metrics_tick` events. It enforces budgets. See `brainstorms/09-metrics.md`.
- **CV profile** (cross-cutting): an optional CV (PDF or MD/TXT only, validated by content) is turned into a `Profile` by the LLM, or by a parser + skills/titles taxonomy fallback. User edits live in `overrides` and win over re-extraction. The profile drives `job_matches.score` (all jobs are stored; relevance is ranking, not filtering), company scope, and frontier scoring. Changing the profile triggers a background re-score. See `brainstorms/12-cv-profile.md`.
- **ui**: an axum server on 127.0.0.1 with the graph (domain nodes, drilling into a page-level subgraph from `page_links`), metrics panel, and natural-language job search. NL search works like this: the LLM turns the text into a structured `JobQuery` filter, Rust builds parameterized SQL from it (geo radius via offline GeoNames, "well paid" as a relative salary percentile), and FTS5 handles keywords. It deliberately avoids free-form text-to-SQL.
