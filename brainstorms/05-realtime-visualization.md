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

**Implemented (milestone 8, `crates/ui/static/`).** No build step: ES modules served from the binary (`include_str!`), with graph libraries from jsDelivr **pinned** (`graphology@0.26.0`, `sigma@3.0.3`, `graphology-layout-forceatlas2@0.10.1`). The UI needs internet for those, which the crawler needs anyway.
- `graph.js`:
  - Snapshots from `/api/graph` (every 15 s, plus 2 s after an event names an unknown domain) are **merged**. New nodes start near a linked neighbour, and ForceAtlas2 runs in short animation-frame bursts; there's no full re-layout.
  - Live events pulse nodes (`page_fetched`, `jobs_found`, `careers_found`) and recolour them (`domain_classified`).
  - Hover or select dims everything outside the node's neighbourhood.
  - Labels only on larger nodes (sigma's label grid).
  - An HTML tooltip replaces sigma's light-only hover box.
  - Theme changes re-read the colour tokens.
- Colours: the dataviz reference palette's first 3 categorical slots, validated all-pairs in light and dark, for company / not sure yet / not a company, plus a neutral for "linked, not fetched". The legend shows counts. Aqua is below 3:1 on light, which the legend and labels offset.
- Side panel: live feed (filters for jobs / careers / classifications / failures / crawler; hosts are clickable) and domain details (classification signals, careers page, boards, open jobs with salary, pages). Search with autocomplete focuses the camera on a domain.
- Resources: four single-series small multiples (download rate, pages/min, CPU, RSS) over 30 minutes, with a crosshair tooltip (pointer or ← →) and a table view. Lines break across crawler runs and gaps over 10 s. Byte units are decimal (kB = 1000), so ticks are round.
- Checked in the browser: dark and light, 375 px wide (no horizontal scroll), live updates while crawling, and no console errors.

**Implemented (milestone 9).**
- **Replay:**
  - `replay` opens a timeline with play/pause, a speed selector (30× … 1 h/s), the replayed time, an activity strip (pages fetched per bucket) with crawler runs underlined in the accent colour, and a scrubber.
  - Each position fetches `/api/graph?at=T` (at most every 350 ms, latest time wins) and merges it with **prune**, so scrubbing backwards removes domains that didn't exist yet, while nodes that persist keep their positions.
  - The tiles show totals at T, and the feed shows the events up to T.
  - Playback **skips idle time between crawler runs**.
  - Live events are ignored while replaying, except crawler status. "Back to live" restores everything.
- **How `/api/graph?at=T` rebuilds the past:** from `domains.first_seen`, `edges.first_seen`, `pages.fetched_at`, job `first_seen`/`closed_at`, and each domain's last `domain_classified` event ≤ T. Careers URL, ATS and edge weights are current values. Live mode reads `domains.status` directly. `/api/history` supplies the runs and activity; migration 0007 indexes `events(ts)` and `events(kind, ts)`.
- **Drill-down:** double-click a domain, or use "explore its pages →" in its details, to switch to a focused page view with a breadcrumb (`web of companies › host`); Esc or the breadcrumb goes back.
  - `/api/domains/{host}/graph` returns up to 500 pages and 200 unfetched internal links (with frontier state), with external links collapsed into up to 40 site nodes.
  - Page colours use three categorical slots (careers page, job posting, other site), neutrals for pages and unfetched links, and the status red for failures.
  - Double-click a page to open it, or an external site to drill into that site.
  - While crawling, the open page view refreshes when its domain's pages are fetched.
  - During a replay it shows current pages and says so in the legend.

### Original plan
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
