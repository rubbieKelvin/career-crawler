-- Periodic resource and progress samples written by the crawler's metrics sampler; the UI
-- tails this table for live charts. Counters (requests, bytes) are cumulative within one
-- crawler run (`run_id` = the id of that run's crawler_started event); gauges and table
-- counts are point-in-time.
CREATE TABLE metrics_samples (
  id             INTEGER PRIMARY KEY,
  ts             INTEGER NOT NULL,
  run_id         INTEGER NOT NULL,
  requests       INTEGER NOT NULL,
  fetch_errors   INTEGER NOT NULL,
  bytes_rx_wire  INTEGER NOT NULL,
  bytes_rx_body  INTEGER NOT NULL,
  bytes_tx       INTEGER NOT NULL,
  bytes_wasted   INTEGER NOT NULL,
  cpu_pct        REAL,
  cpu_time_ms    INTEGER,
  rss_bytes      INTEGER,
  db_bytes       INTEGER,
  in_flight      INTEGER NOT NULL,
  frontier_queued INTEGER NOT NULL,
  pages          INTEGER NOT NULL,
  domains        INTEGER NOT NULL,
  companies      INTEGER NOT NULL,
  jobs           INTEGER NOT NULL
);
CREATE INDEX metrics_samples_ts ON metrics_samples(ts);
