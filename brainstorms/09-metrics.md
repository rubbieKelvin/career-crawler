# 09 — Metrics: network, compute, memory, crawl health

We need to know what the crawler costs (bandwidth, CPU, RAM, disk) and how well it's working, both **live** in the UI and **historically** in the DB.

## What to track

### Network
| Metric | How |
|---|---|
| Bytes received (wire, compressed) | Turn off reqwest's automatic decompression. Count bytes as the body streams in, then decompress ourselves (`async-compression`). Otherwise we can only see decompressed size, which overstates bandwidth. |
| Bytes received (decompressed) | Count after decompression. The compression ratio is a useful extra number. |
| Bytes sent (approx.) | Request line + headers + body size. Good enough; exact TCP/TLS overhead isn't worth chasing. |
| Per-domain / per-ATS bytes | Add to `domains` / aggregate by `source`. Shows which sites are expensive. |
| Requests by status class, errors by kind (timeout, DNS, TLS, too-large, robots-denied) | Counters |
| Fetch latency (TTFB + total) | Histogram (p50/p95/p99) |
| Wasted bytes | Bytes spent on pages that were dropped (non-HTML, trap, duplicate hash). |

### Compute & memory (process-level)
| Metric | How |
|---|---|
| CPU % and total CPU time | `sysinfo` crate, refresh this process every ~1s |
| RSS / virtual memory | `sysinfo` (or `memory-stats`, which is lighter) |
| Heap bytes allocated / in use | `tikv-jemallocator` + `tikv-jemalloc-ctl` (`stats.allocated`, `stats.resident`). An optional feature flag, so the default build stays simple. |
| Tokio runtime | `tokio-metrics` (task poll times, busy ratio, queue depth). Needs `--cfg tokio_unstable` for some fields; put it behind a feature. |
| Open file descriptors / sockets | `sysinfo` or `/proc` on Linux; best-effort on macOS |

### LLM & headless browser
- LLM: calls, tokens in/out, estimated cost, cache hit rate, errors, latency, and use of the daily token budget (see `10-llm.md`).
- Browser: Chrome child-process RSS/CPU (sum over the process tree via `sysinfo`), pages rendered, render latency, and bytes via CDP `encodedDataLength`.

### Storage
- SQLite file size + WAL size (stat the files), row counts per table, and write-queue depth for the single writer task.

### Crawl effectiveness
- Pages/sec, frontier size, in-flight requests, domains discovered/scored/companies, careers pages found, jobs found (new vs updated).
- **Efficiency ratios**: bytes per job found, pages per company found, and the share of companies that have a careers page. These tell us whether scoring changes are actually helping.

**Implemented (milestone 7)**, with one change from the plan below: there are **no `metrics_tick` events**.
- The sampler (`crawler/src/sampler.rs`) writes a `metrics_samples` row every `metrics_interval_secs` (default 2), and the UI tails that table directly. This keeps the events table for crawl events.
- Each sample holds:
  - this run's cumulative counters (requests, errors, bytes rx wire/body, tx, wasted)
  - process CPU %, accumulated CPU ms and RSS (`sysinfo`)
  - DB + WAL + SHM size
  - the in-flight gauge
  - table counts
- `run_id` = the run's `crawler_started` event id. Rates come from consecutive samples within a run (`samples::rates`).
- Budget: `max_bytes` (0 = none) makes the sampler apply a `stop` with source `budget:max_bytes`.
- Not done yet: `max_bytes_per_domain`, `max_rss_bytes`, bandwidth limiting, jemalloc/tokio-metrics, LLM token counters, Prometheus export.

## Architecture
- A **`metrics` module** with one `Metrics` struct of atomics (`AtomicU64` counters, plus a small histogram such as `hdrhistogram` behind a mutex), shared as `Arc<Metrics>`. The fetcher, frontier and store increment it; nothing blocks.
  - Alternative: the `metrics` crate facade + `metrics-exporter-prometheus`. This gives a `/metrics` Prometheus endpoint for free. Could do both: atomics internally, and export to Prometheus as well.
- A **sampler task** that runs every 1s:
  1. Reads the system stats (CPU, RSS, jemalloc) and the atomics.
  2. Computes rates (bytes/s, pages/s) from the previous sample.
  3. Emits a `metrics_tick` event into the `events` table. The separate UI process tails that table and charts it live.
  4. Every 10s (configurable), persists the sample to SQLite.

```sql
CREATE TABLE metrics_samples (
  ts               INTEGER PRIMARY KEY,
  bytes_rx_wire    INTEGER, bytes_rx_body INTEGER, bytes_tx INTEGER,  -- cumulative
  requests         INTEGER, errors INTEGER,
  cpu_pct          REAL, cpu_time_ms INTEGER,
  rss_bytes        INTEGER, heap_bytes INTEGER,
  db_bytes         INTEGER,
  frontier_size    INTEGER, in_flight INTEGER,
  pages            INTEGER, domains INTEGER, companies INTEGER, jobs INTEGER
);
```
Store cumulative counters and derive rates when querying, so a missed sample doesn't corrupt totals. On restart, load the last row so totals continue across runs; also track a `run_id` if per-run numbers are wanted.

## UI / routes
- `GET /api/metrics`: current snapshot (JSON).
- `GET /api/metrics/history?from=&to=&step=`: downsampled series from `metrics_samples`.
- `GET /metrics`: Prometheus text format (optional).
- UI: a metrics panel next to the graph, with a KPI row (total data, current bandwidth, CPU, RAM, jobs, bytes/job) and sparklines/time-series for bandwidth, CPU, RSS and pages/s.

## Budgets (metrics that feed back into control)
Config limits checked by the sampler, which pauses or stops the crawl and emits an event when one is hit:
- `max_bytes` (a total data cap, useful on metered connections), `max_bytes_per_domain`
- `max_rss_bytes`: shrink the in-memory frontier cache, or pause
- `max_bandwidth_bps`: a global rate limiter on body reads (`governor` on bytes)
