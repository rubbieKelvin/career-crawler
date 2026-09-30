# 04 — SQLite storage schema (draft)

Use WAL mode. Migrations via `sqlx migrate` (`migrations/NNNN_name.sql`).

```sql
CREATE TABLE domains (
  id              INTEGER PRIMARY KEY,
  host            TEXT UNIQUE NOT NULL,     -- registrable domain
  company_score   REAL,
  score_reasons   TEXT,                     -- JSON
  status          TEXT NOT NULL,            -- discovered|probing|company|not_company|harvested|blocked
  careers_url     TEXT,
  jobs_api_url    TEXT,                     -- discovered via headless network interception
  industry        TEXT,
  hq_country      TEXT,
  ats             TEXT,                     -- greenhouse|lever|...|NULL
  ats_token       TEXT,
  first_seen      INTEGER NOT NULL,
  last_crawled    INTEGER
);

CREATE TABLE pages (
  id            INTEGER PRIMARY KEY,
  url           TEXT UNIQUE NOT NULL,       -- normalized
  domain_id     INTEGER NOT NULL REFERENCES domains(id),
  kind          TEXT,                       -- home|careers|job|other
  http_status   INTEGER,
  fetched_at    INTEGER,
  content_hash  TEXT,
  rendered      INTEGER NOT NULL DEFAULT 0, -- 1 = fetched via headless browser
  bytes_wire    INTEGER,
  error         TEXT
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

-- page-level links, for drilling into one domain's subgraph
CREATE TABLE page_links (
  src_page_id   INTEGER NOT NULL REFERENCES pages(id),
  dst_url       TEXT NOT NULL,              -- may not be fetched (yet)
  dst_page_id   INTEGER REFERENCES pages(id),
  anchor_text   TEXT,
  link_score    REAL,
  PRIMARY KEY (src_page_id, dst_url)
);

-- domain-level graph edges for the "net" visual (rolled up from page_links)
CREATE TABLE edges (
  src_domain_id INTEGER NOT NULL REFERENCES domains(id),
  dst_domain_id INTEGER NOT NULL REFERENCES domains(id),
  weight        INTEGER NOT NULL DEFAULT 1,
  first_seen    INTEGER NOT NULL,
  PRIMARY KEY (src_domain_id, dst_domain_id)
);

CREATE TABLE jobs (
  id              INTEGER PRIMARY KEY,
  domain_id       INTEGER NOT NULL REFERENCES domains(id),
  url             TEXT NOT NULL,
  title           TEXT NOT NULL,
  location        TEXT,
  remote          INTEGER,
  department      TEXT,
  employment_type TEXT,
  category        TEXT, seniority TEXT, skills TEXT,       -- enrichment (JSON array for skills)
  city TEXT, region TEXT, country_code TEXT, lat REAL, lon REAL,
  remote_mode     TEXT,                                    -- onsite|hybrid|remote
  salary_min      REAL, salary_max REAL, salary_currency TEXT,
  salary_usd_annual REAL,
  posted_at       INTEGER,
  description     TEXT,
  source          TEXT NOT NULL,
  first_seen      INTEGER NOT NULL,
  last_seen       INTEGER NOT NULL,
  closed_at       INTEGER,
  UNIQUE (domain_id, url)
);

-- append-only event log: powers history replay in the UI
CREATE TABLE events (
  id        INTEGER PRIMARY KEY,
  ts        INTEGER NOT NULL,
  kind      TEXT NOT NULL,
  payload   TEXT NOT NULL      -- JSON, same shape as the live WS message
);

CREATE INDEX jobs_geo ON jobs(lat, lon);

-- UI -> crawler commands (see 11-process-architecture.md)
CREATE TABLE control_commands (
  id INTEGER PRIMARY KEY, ts INTEGER NOT NULL,
  command TEXT NOT NULL, args TEXT, status TEXT NOT NULL DEFAULT 'pending'
);

-- LLM cache + audit (see 10-llm.md)
CREATE TABLE llm_calls (
  id INTEGER PRIMARY KEY, ts INTEGER NOT NULL,
  task TEXT NOT NULL, prompt_version TEXT NOT NULL, model TEXT NOT NULL,
  input_hash TEXT NOT NULL, output TEXT, error TEXT,
  tokens_in INTEGER, tokens_out INTEGER, cost_usd REAL, latency_ms INTEGER,
  UNIQUE (task, prompt_version, model, input_hash)
);
```

Reference data: `geonames_cities` (offline, loaded once) and `fx_rates`.

Needed now (was optional): an FTS5 virtual table over `jobs(title, description, skills)` for keyword search in NL queries.

Crawler and UI are separate processes (WAL mode, `busy_timeout`). Crawler writes: funnel through a single writer task (mpsc channel) to avoid SQLITE_BUSY contention; readers use a pool.
