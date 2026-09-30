-- LLM layer (milestone 10).

-- Every provider call, successful or not: the response cache and the audit/cost log.
-- `cache_key` = blake3(task, prompt version, model, input); only `ok` rows are cache hits,
-- so a bad answer is retried next time. Cache hits are counted in metrics, not logged here.
CREATE TABLE llm_calls (
  id             INTEGER PRIMARY KEY,
  ts             INTEGER NOT NULL,
  task           TEXT NOT NULL,
  prompt_version INTEGER NOT NULL,
  model          TEXT NOT NULL,
  cache_key      TEXT NOT NULL,
  status         TEXT NOT NULL,               -- ok | parse_error | error
  output         TEXT,                        -- the model's reply (ok / parse_error)
  error          TEXT,
  tokens_in      INTEGER NOT NULL DEFAULT 0,
  tokens_out     INTEGER NOT NULL DEFAULT 0,
  cost_usd       REAL NOT NULL DEFAULT 0,
  latency_ms     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX llm_calls_cache ON llm_calls(cache_key, status);
CREATE INDEX llm_calls_ts ON llm_calls(ts);

-- The LLM's opinion on a gray-zone domain (asked at most once per domain).
ALTER TABLE domains ADD COLUMN llm_checked_at INTEGER;
ALTER TABLE domains ADD COLUMN llm_verdict TEXT;   -- JSON

-- Job enrichment. `salary_annual_*` are the salary normalized to a year in the posting's
-- own currency (the raw `salary_*` columns stay as the source gave them, so a re-crawl
-- doesn't fight the enricher); `salary_usd_annual` already existed.
-- `enrich_state`: pending (never enriched, or the posting changed) -> rules (deterministic
-- pass done, the LLM could still help) -> done. The upsert resets it when the posting changes.
ALTER TABLE jobs ADD COLUMN salary_annual_min REAL;
ALTER TABLE jobs ADD COLUMN salary_annual_max REAL;
ALTER TABLE jobs ADD COLUMN remote_regions TEXT;   -- JSON array of regions a remote job is open to
ALTER TABLE jobs ADD COLUMN enrich_state TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE jobs ADD COLUMN enrich_attempts INTEGER NOT NULL DEFAULT 0;
CREATE INDEX jobs_enrich ON jobs(enrich_state);

-- LLM counters, cumulative per crawler run like the byte counters.
ALTER TABLE metrics_samples ADD COLUMN llm_calls INTEGER NOT NULL DEFAULT 0;
ALTER TABLE metrics_samples ADD COLUMN llm_cache_hits INTEGER NOT NULL DEFAULT 0;
ALTER TABLE metrics_samples ADD COLUMN llm_errors INTEGER NOT NULL DEFAULT 0;
ALTER TABLE metrics_samples ADD COLUMN llm_tokens_in INTEGER NOT NULL DEFAULT 0;
ALTER TABLE metrics_samples ADD COLUMN llm_tokens_out INTEGER NOT NULL DEFAULT 0;
