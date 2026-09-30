# 05 — Real-time visualization

## Routes (axum)
| Route | Purpose |
|---|---|
| `GET /` | static SPA (single `index.html` + JS, embedded via `include_str!`/`rust-embed`) |
| `GET /api/stats` | counters: pages, domains, companies, jobs, frontier size, pages/sec |
| `GET /api/graph?since=` | snapshot of nodes + edges for initial render |
| `GET /api/events?after_id=` | history replay from `events` table |
| `GET /api/jobs?q=` | job listing / search |
| `GET /api/domains/:host` | domain detail (score reasons, careers url, jobs) |
| `GET /ws` (or `/sse`) | live event stream |
| `POST /api/control/{pause,resume}` | crawl control (nice to have) |

## Event shape
```json
{ "id": 123, "ts": 1759200000, "kind": "page_fetched",
  "data": { "url": "...", "domain": "acme.com", "status": 200, "from": "vc.com" } }
```
Kinds: `url_enqueued, page_fetched, fetch_failed, domain_scored, careers_found, ats_detected, jobs_found, domain_blocked, crawler_stats`.

Same struct serialized to WS and to `events` table → history replay and live view share one code path in the frontend.

Backpressure: `broadcast` channel drops for slow clients (`RecvError::Lagged`) → client refetches snapshot. Throttle `url_enqueued` (very chatty) or batch events every ~200ms.

## The "net" visual
- **Nodes = domains** (page-level graph explodes too fast). Size = pages crawled, color = status (grey discovered, blue company, green has jobs, red blocked), badge = job count.
- **Edges = domain links** (`edges` table), thickness = weight.
- Currently-in-flight domains pulse.
- Library options: **sigma.js + graphology** (WebGL, scales to 10k+ nodes) ← preferred; `force-graph` (canvas, easy); cytoscape.js (rich but slower at scale).
- Side panels: live log feed, stats, clicked-node detail with jobs list.
- **History mode**: timeline scrubber replays `events` into the graph.

Keep frontend dependency-free of a build step if possible (CDN scripts + vanilla JS module) so `cargo run` is the only command.
