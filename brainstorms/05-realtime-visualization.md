# 05 — Real-time visualization

Lives in the **`ui` binary**, a separate process from the crawler. Live events come from tailing the `events` table (see `11-process-architecture.md`). It binds 127.0.0.1 only.

**Implemented (milestone 7, `crates/ui`).**
- The tailer (`live.rs`) checks `events` and `metrics_samples` for new rows every 200 ms and broadcasts to WS clients:
  - `{"type":"event","id","ts","event"}`
  - `{"type":"metrics","sample","rates"}`
  - `{"type":"lagged","skipped"}` when a slow client missed messages; it should refetch `/api/stats` and `/api/graph`
- The tailer starts after what's already stored; history comes from REST.
- Crawler status (`/api/stats` → `crawler`):
  - `running`: the latest `crawler_started` is newer than the latest `crawler_stopped` *and* a metrics sample arrived in the last 15 s, which catches crashed crawlers
  - `paused`: the latest pause/resume `control_applied` event since the run started
- `/api/graph` returns the top-N domains by `pages*3 + open jobs + degree` (default 1000, max 10000; `?discovered=false` drops never-fetched domains) and only the edges between them.
- `/` is an interim debug page (counts, rates, controls, live event log) until milestone 8.

## Routes (axum)
| Route | Purpose |
|---|---|
| `GET /` | static SPA (single `index.html` + JS, embedded via `include_str!`/`rust-embed`) |
| `GET /api/stats` | counts, crawler status (running/paused), latest metrics sample + rates |
| `GET /api/graph?limit=&discovered=` | snapshot of domain nodes + edges for initial render |
| `GET /api/events?after_id=&limit=` | history replay from `events` (no `after_id`: the latest `limit`) |
| `GET /api/jobs?q=` | job listing / search |
| `GET /api/domains/:host` | domain detail (score reasons, careers url, jobs) |
| `GET /api/domains/:host/graph` | page-level subgraph for one domain (from `page_links`) |
| `POST /api/search/nl` | natural-language job search (see `10-llm.md`) |
| `POST /api/profile/cv`, `GET/PUT /api/profile` | upload a CV (PDF/MD/TXT), view/edit the profile (see `12-cv-profile.md`) |
| `GET /api/jobs/matches` | jobs ranked by `match_score` for the active profile |
| `GET /api/metrics`, `/api/metrics/history` | resource + crawl metrics (see `09-metrics.md`) |
| `GET /ws` (or `/sse`) | live event stream |
| `POST /api/control/{pause,resume,stop}` | crawl control, written to `control_commands` (202 Accepted); `headless` comes with milestone 13 |

## Event shape
```json
{ "id": 123, "ts": 1759200000, "kind": "page_fetched",
  "data": { "url": "...", "domain": "acme.com", "status": 200, "from": "vc.com" } }
```
Kinds: `url_enqueued, page_fetched, fetch_failed, domain_scored, careers_found, ats_detected, jobs_found, domain_blocked, metrics_tick, budget_exceeded`.

Same struct serialized to WS and to `events` table → history replay and live view share one code path in the frontend.

Backpressure: `broadcast` channel drops for slow clients (`RecvError::Lagged`) → client refetches snapshot. Throttle `url_enqueued` (very chatty) or batch events every ~200ms.

## The "net" visual
- **Nodes = domains** (page-level graph explodes too fast). Size = pages crawled, color = status (grey discovered, blue company, green has jobs, red blocked), badge = job count.
- **Edges = domain links** (`edges` table), thickness = weight.
- Currently-in-flight domains pulse.
- Library options: **sigma.js + graphology** (WebGL, scales to 10k+ nodes) ← preferred; `force-graph` (canvas, easy); cytoscape.js (rich but slower at scale).
- Side panels: live log feed, stats, clicked-node detail with jobs list.
- **Drill-down into pages (decided)**: double-click a domain node to expand it **in place** into its page subgraph. Pages are laid out around the domain's position, and the rest of the graph dims. Or open it in a focused side view (try both).
  - Page nodes: color by `kind` (home / careers / job / other), a ring if `rendered` by headless, and size by link score.
  - Edges: `page_links` inside the domain. Links that leave the domain collapse into one edge per external domain.
  - Live: while the domain is expanded, the UI subscribes to that domain's `page_fetched` events so new pages pop in.
  - Collapse with Esc or a breadcrumb (`all domains › acme.com`).
- **History mode**: timeline scrubber replays `events` into the graph.

- **Profile panel**: CV upload, extracted profile as editable chips, a "for you" ranked job list with match reasons. The graph can color domains by the best match score among their jobs.
- **Search panel**: an NL search box. Results show as a list, the interpreted filter shows as editable chips, and matching jobs' domains get highlighted in the graph.

Keep the frontend free of a build step if possible (CDN scripts + a vanilla JS module), so `cargo run -p ui` is the only command.
