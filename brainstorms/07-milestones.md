# 07 — Milestones (build order)

1. **Skeleton** — tokio + tracing + clap config; SQLite with migrations; `seeds.txt` loader.
2. **Fetch & parse** — reqwest client, URL normalization, link extraction, robots.txt, per-host rate limit.
3. **Frontier** — DB-backed priority queue, heuristic link scoring, per-domain budgets. Crawl runs end-to-end (no jobs yet).
4. **Company scoring** — domain classifier, discovery vs harvest mode.
5. **Careers detection** — anchor vocab, well-known paths, ATS link detection.
6. **Job extraction** — Greenhouse + Lever + Ashby providers, JSON-LD parser. Jobs table populated.
7. **Event bus + web server + metrics**: axum, `/ws`, `/api/stats`, `/api/graph`, the metrics sampler and `/api/metrics`. The byte counters in the fetcher should already exist from milestone 2.
8. **Graph UI**: sigma.js live graph, stats, log feed, node detail, metrics panel (bandwidth/CPU/RAM charts).
9. **History replay** — events table scrubber.
10. **Hardening** — trap detection, backoff, more ATS providers, HTML heuristic extractor, optional headless browser.

Each milestone should be runnable and tested (fixtures: saved HTML pages under `tests/fixtures/` so classifiers/extractors test offline).
