-- Set once the well-known careers paths and sitemap have been tried for a company domain,
-- so the probes run at most once per domain.
ALTER TABLE domains ADD COLUMN careers_probed INTEGER NOT NULL DEFAULT 0;
