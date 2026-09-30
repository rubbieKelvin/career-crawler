-- Core crawl schema. All timestamps are unix epoch milliseconds.
-- Later milestones add their own migrations (metrics, llm, profiles, fts);
-- see brainstorms/04-storage-schema.md for the full plan.

CREATE TABLE domains (
  id              INTEGER PRIMARY KEY,
  host            TEXT NOT NULL UNIQUE,        -- registrable domain
  company_score   REAL,
  score_reasons   TEXT,                        -- JSON
  status          TEXT NOT NULL DEFAULT 'discovered',
                  -- discovered|probing|company|not_company|harvested|blocked
  careers_url     TEXT,
  jobs_api_url    TEXT,
  ats             TEXT,
  ats_token       TEXT,
  industry        TEXT,
  hq_country      TEXT,
  first_seen      INTEGER NOT NULL,
  last_crawled    INTEGER
);

CREATE TABLE pages (
  id            INTEGER PRIMARY KEY,
  url           TEXT NOT NULL UNIQUE,          -- normalized
  domain_id     INTEGER NOT NULL REFERENCES domains(id),
  kind          TEXT,                          -- home|careers|job|other
  http_status   INTEGER,
  fetched_at    INTEGER,
  content_hash  TEXT,
  rendered      INTEGER NOT NULL DEFAULT 0,    -- 1 = fetched via headless browser
  bytes_wire    INTEGER,
  error         TEXT
);
CREATE INDEX pages_domain ON pages(domain_id);

CREATE TABLE page_links (
  src_page_id   INTEGER NOT NULL REFERENCES pages(id),
  dst_url       TEXT NOT NULL,
  dst_page_id   INTEGER REFERENCES pages(id),
  anchor_text   TEXT,
  link_score    REAL,
  PRIMARY KEY (src_page_id, dst_url)
);

CREATE TABLE edges (
  src_domain_id INTEGER NOT NULL REFERENCES domains(id),
  dst_domain_id INTEGER NOT NULL REFERENCES domains(id),
  weight        INTEGER NOT NULL DEFAULT 1,
  first_seen    INTEGER NOT NULL,
  PRIMARY KEY (src_domain_id, dst_domain_id)
);

CREATE TABLE frontier (
  url           TEXT PRIMARY KEY,
  score         REAL NOT NULL,
  depth         INTEGER NOT NULL,
  from_page_id  INTEGER REFERENCES pages(id),
  reason        TEXT,
  state         TEXT NOT NULL DEFAULT 'queued', -- queued|in_flight|done|skipped
  enqueued_at   INTEGER NOT NULL
);
CREATE INDEX frontier_ready ON frontier(state, score DESC);

CREATE TABLE jobs (
  id                INTEGER PRIMARY KEY,
  domain_id         INTEGER NOT NULL REFERENCES domains(id),
  url               TEXT NOT NULL,
  title             TEXT NOT NULL,
  location          TEXT,
  department        TEXT,
  employment_type   TEXT,
  category          TEXT,
  seniority         TEXT,
  skills            TEXT,                      -- JSON array
  city              TEXT,
  region            TEXT,
  country_code      TEXT,
  lat               REAL,
  lon               REAL,
  remote_mode       TEXT,                      -- onsite|hybrid|remote
  salary_min        REAL,
  salary_max        REAL,
  salary_currency   TEXT,
  salary_usd_annual REAL,
  posted_at         INTEGER,
  description       TEXT,
  source            TEXT NOT NULL,             -- ats:<name>|jsonld|heuristic|llm
  first_seen        INTEGER NOT NULL,
  last_seen         INTEGER NOT NULL,
  closed_at         INTEGER,
  UNIQUE (domain_id, url)
);
CREATE INDEX jobs_geo ON jobs(lat, lon);

-- Append-only; the UI process tails this table for live updates and history replay.
CREATE TABLE events (
  id        INTEGER PRIMARY KEY,
  ts        INTEGER NOT NULL,
  kind      TEXT NOT NULL,
  payload   TEXT NOT NULL                      -- full JSON event, same shape sent to UI clients
);

-- UI -> crawler commands; the crawler polls pending rows.
CREATE TABLE control_commands (
  id        INTEGER PRIMARY KEY,
  ts        INTEGER NOT NULL,
  command   TEXT NOT NULL,
  args      TEXT,
  status    TEXT NOT NULL DEFAULT 'pending'    -- pending|done|failed
);
