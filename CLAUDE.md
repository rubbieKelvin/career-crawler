# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

Greenfield Rust crate (`career`, edition 2024). `src/main.rs` is still the cargo hello-world stub. Design notes live in `brainstorms/` (numbered `NN-topic.md`); read `brainstorms/00-overview.md` first — it is the source of truth for intent until real code exists. When a brainstorm decision is implemented or overturned, update the brainstorm rather than letting it drift.

## What this program is

A recursive web crawler whose goal is finding **company career pages and the job postings on them**, storing results in a local SQLite database. It must:

1. Decide *which* sites are worth crawling (prioritized frontier, not a blind BFS).
2. On a site judged reputable/company-like, locate its careers/jobs link and crawl that section.
3. Extract job postings (schema.org `JobPosting` JSON-LD first, known ATS APIs second, HTML heuristics last).
4. Serve a web UI that shows the live crawl and the crawl history as a network graph, updated in real time.

## Commands

```bash
cargo build
cargo run                      # run the crawler + web UI
cargo test                     # all tests
cargo test <name_substring>    # single test / group
cargo clippy --all-targets -- -D warnings
cargo fmt
```

## Intended architecture

Planned module boundaries (see brainstorms for rationale). Keep these seams even when files are small — the web UI and the crawler must communicate only through the event bus and the DB.

- **frontier** — priority queue of URLs, keyed by a score. Dedups by normalized URL; enforces per-host politeness (robots.txt, rate limit, concurrency cap).
- **fetcher** — async HTTP (`reqwest`), size/time limits, content-type filtering.
- **classifier / scoring** — scores domains ("is this a company site?") and links ("does this lead toward careers?"). Pure functions over parsed page data so they're unit-testable without the network.
- **extractor** — career-link discovery and job-posting extraction, including ATS detectors (Greenhouse, Lever, Ashby, Workday, …).
- **store** — SQLite persistence (domains, pages, edges, jobs, crawl events). The crawl must be resumable from the DB.
- **events** — a `tokio::sync::broadcast` channel of crawl events; every state change is emitted here and persisted.
- **metrics**: a shared `Arc<Metrics>` of atomics (bytes on the wire and decompressed, requests, errors, latency) that the fetcher, frontier and store increment. A 1s sampler task adds process CPU/RSS/heap readings, emits `metrics_tick` events, persists to `metrics_samples`, and enforces budgets (data cap, memory cap). Wire bytes need reqwest's auto-decompression turned off, with decompression done manually after counting. See `brainstorms/09-metrics.md`.
- **web** — `axum` server: REST for history/snapshot, WebSocket/SSE for live events, static frontend that renders the graph.

Data flow: `frontier → fetcher → parser → (classifier, extractor) → store + events → frontier (new scored links)`.
