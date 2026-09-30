-- CV profiles (milestone 11, brainstorms/12-cv-profile.md). One profile is active at a time.
-- `extracted` is what the CV said (by the LLM or the parser); `overrides` are the user's edits,
-- kept apart so re-extracting never wipes them. `matched_at` is the `updated_at` the job matches
-- were last computed for: `matched_at < updated_at` means the ranking is stale.
CREATE TABLE profiles (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL,
  active      INTEGER NOT NULL DEFAULT 0,
  cv_hash     TEXT,
  cv_text     TEXT,
  source      TEXT NOT NULL,                 -- llm | parser
  extracted   TEXT NOT NULL,                 -- JSON Profile
  overrides   TEXT NOT NULL DEFAULT '{}',    -- JSON Overrides
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL,
  matched_at  INTEGER
);
CREATE INDEX profiles_hash ON profiles(cv_hash);

-- How well each job fits each profile. Every job is stored; relevance is a rank, not a filter.
CREATE TABLE job_matches (
  profile_id  INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
  job_id      INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  score       REAL NOT NULL,
  reasons     TEXT,                          -- JSON array of short strings
  llm_fit     REAL,
  llm_why     TEXT,
  computed_at INTEGER NOT NULL,
  PRIMARY KEY (profile_id, job_id)
);
CREATE INDEX job_matches_rank ON job_matches(profile_id, score DESC);

-- The part of a queued URL's score that came from the profile (topic, region and department
-- terms), so a profile change can swap it without re-deriving the rest of the score.
ALTER TABLE frontier ADD COLUMN profile_boost REAL NOT NULL DEFAULT 0;
-- Looked up by destination when a profile change re-scores queued links (their anchor text).
CREATE INDEX page_links_dst ON page_links(dst_url);
