# 07 — Milestones (build order)

1. **Skeleton**: Cargo workspace (`core`, `llm`, `crawler`, `ui`), tokio + tracing + clap config, SQLite with migrations in `core`, `seeds.txt` loader.
2. **Fetch & parse** — reqwest client, URL normalization, link extraction, robots.txt, per-host rate limit.
3. **Frontier** — DB-backed priority queue, heuristic link scoring, per-domain budgets. Crawl runs end-to-end (no jobs yet).
4. **Company scoring** — domain classifier, discovery vs harvest mode.
5. **Careers detection** — anchor vocab, well-known paths, ATS link detection.
6. **Job extraction** — Greenhouse + Lever + Ashby providers, JSON-LD parser. Jobs table populated.
7. **Event feed + UI process + metrics**: the crawler writes `events`, and the `ui` binary tails them. axum, `/ws`, `/api/stats`, `/api/graph`, the metrics sampler and `/api/metrics`. The byte counters in the fetcher should already exist from milestone 2.
8. **Graph UI**: sigma.js live graph, stats, log feed, node detail, metrics panel (bandwidth/CPU/RAM charts).
9. **History replay + drill-down**: events-table scrubber, page-level subgraph per domain.
10. **LLM layer**: provider client + cache, gray-zone domain classification, job enrichment (geo, salary, category).
11. **CV profile**: upload/CLI, PDF/MD extraction (LLM + parser fallback), `job_matches` scoring, profile-aware frontier scoring, profile panel.
12. **NL search**: `POST /api/search/nl`, a `JobQuery` → SQL builder, search panel with filter chips.
13. **Headless browser**: `headless` feature, browser pool, SPA detection, capturing the jobs API from network traffic.
14. **Hardening**: trap detection, backoff, more ATS providers, the HTML heuristic extractor, local embeddings.

Each milestone should be runnable and tested (fixtures: saved HTML pages under `tests/fixtures/` so classifiers/extractors test offline).
