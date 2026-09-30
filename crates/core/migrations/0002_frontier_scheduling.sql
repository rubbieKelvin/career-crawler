-- Scheduling columns so the dispatcher can pick the best URL per host and enforce
-- per-domain budgets without re-parsing URLs. Rows from before this migration get
-- host/domain backfilled by `frontier::backfill_hosts` at crawler startup.
ALTER TABLE frontier ADD COLUMN host TEXT;
ALTER TABLE frontier ADD COLUMN domain TEXT;
ALTER TABLE frontier ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;

DROP INDEX frontier_ready;
CREATE INDEX frontier_ready ON frontier(state, host, score DESC);
CREATE INDEX frontier_domain ON frontier(domain, state);
