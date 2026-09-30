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

-- graph edges for the "net" visual (domain-level keeps it readable)
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
  salary_min      REAL, salary_max REAL, salary_currency TEXT,
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
```

Optional: FTS5 virtual table over `jobs(title, description)` for search in the UI.

Writes: funnel through a single writer task (mpsc channel) to avoid SQLITE_BUSY contention; readers use a pool.
