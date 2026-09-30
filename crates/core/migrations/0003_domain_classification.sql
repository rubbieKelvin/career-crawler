-- Display name for a domain (schema.org Organization name or og:site_name), for UI labels.
ALTER TABLE domains ADD COLUMN name TEXT;
-- Whether the domain's main homepage (`domain/` or `www.domain/`) has been fetched:
-- only then can a low score conclude "not a company".
ALTER TABLE domains ADD COLUMN home_seen INTEGER NOT NULL DEFAULT 0;
