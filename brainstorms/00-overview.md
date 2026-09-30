# 00 — Overview

## Goal
Crawl the web recursively, find **companies**, find their **careers page**, and extract **job postings** into a local SQLite DB. Watch it all happen live in a browser, as a network graph.

## Core loop
```
seed URLs
   │
   ▼
frontier (priority queue) ──► fetch ──► parse ──┬─► score domain (is it a company?)
   ▲                                             ├─► find careers link (if company)
   │                                             ├─► extract jobs (if careers page)
   └──────── score & enqueue new links ◄─────────┘
                                   │
                                   ▼
                      SQLite  +  event bus ──► web UI (live graph)
```

## Guiding principles
- **Best-first, not breadth-first.** The web is infinite; the only thing that matters is what we crawl *next*. Every URL gets a score. See `01-crawl-strategy.md`.
- **Structured data before heuristics.** schema.org `JobPosting` and ATS JSON APIs are far more reliable than scraping HTML. See `03-job-extraction.md`.
- **Everything is an event.** The UI, the DB history and debugging all come from the same event stream. See `05-realtime-visualization.md`.
- **Be a polite crawler.** robots.txt, per-host rate limits, identifiable User-Agent. See `06-politeness-and-safety.md`.
- **Resumable.** Kill it, restart it, it continues from the DB.
- **Two processes, one DB.** The `crawler` and `ui` binaries share SQLite (WAL). See `11-process-architecture.md`.
- **LLM for the ambiguous cases, not everything.** DeepSeek via an OpenAI-compatible client for classification, extraction, and NL job search. See `10-llm.md`.
- **Local only.** No auth, and the UI binds localhost.

## Proposed crates
| Concern | Crate |
|---|---|
| Async runtime | `tokio` |
| HTTP | `reqwest` (rustls, gzip/brotli) |
| HTML parsing | `scraper` (or `lol_html` for streaming) |
| URLs | `url`, `publicsuffix`/`psl` for registrable domain |
| SQLite | `sqlx` (async, migrations) or `rusqlite` + `spawn_blocking` |
| Web server | `axum` + `tower-http` (static files) |
| Live updates | `axum` WebSocket or SSE, `tokio::sync::broadcast` |
| robots.txt | `texting_robots` |
| Rate limiting | `governor` |
| Config / CLI | `clap`, `serde`, `toml` |
| Logging | `tracing`, `tracing-subscriber` |
| Headless browser | `chromiumoxide` (feature `headless`, runtime flag) |
| LLM | `reqwest` OpenAI-compatible client, `schemars` for JSON schemas |
| Geo | offline GeoNames cities dataset |
| Process metrics | `sysinfo`, optional `tikv-jemalloc-ctl`, `tokio-metrics` |
| Metrics export | `metrics` + `metrics-exporter-prometheus` (optional) |

## Files in this folder
- `01-crawl-strategy.md` — frontier, scoring, deciding what to crawl
- `02-career-page-detection.md` — reputable-site detection and finding the careers link
- `03-job-extraction.md` — getting postings out of pages and ATSs
- `04-storage-schema.md` — SQLite tables
- `05-realtime-visualization.md` — the live graph UI
- `06-politeness-and-safety.md` — robots, limits, traps
- `07-milestones.md` — build order
- `08-open-questions.md`: decisions made + what's still open
- `10-llm.md`: LLM provider abstraction, classification, enrichment, natural-language search
- `11-process-architecture.md`: the crawler/UI split, the cross-process event feed, control commands
- `09-metrics.md`: bandwidth, CPU, memory, storage and crawl-efficiency metrics; budgets
