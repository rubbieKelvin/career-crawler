-- ATS job boards (vendor + company token). A board may be found before we know which
-- company domain it belongs to (a portfolio page linking to it), so `domain_id` is
-- filled in once the board is attributed.
CREATE TABLE boards (
  key           TEXT PRIMARY KEY,               -- vendor/token, e.g. greenhouse/acme
  vendor        TEXT NOT NULL,
  token         TEXT NOT NULL,
  host          TEXT NOT NULL,
  domain_id     INTEGER REFERENCES domains(id),
  name          TEXT,                           -- company name as the ATS reports it
  first_seen    INTEGER NOT NULL,
  last_fetched  INTEGER,
  last_status   TEXT,                           -- ok | not_found | http_<n> | <fetch error kind> | parse_error
  job_count     INTEGER
);

-- Rebuilt, as it has held no data so far: `url` is the posting's identity, `domain_id`
-- is nullable (board jobs of not-yet-attributed boards), and board/source fields are added.
DROP INDEX jobs_geo;
DROP TABLE jobs;
CREATE TABLE jobs (
  id                INTEGER PRIMARY KEY,
  url               TEXT NOT NULL UNIQUE,
  domain_id         INTEGER REFERENCES domains(id),
  board_key         TEXT REFERENCES boards(key),
  external_id       TEXT,
  title             TEXT NOT NULL,
  company           TEXT,
  location          TEXT,
  department        TEXT,
  employment_type   TEXT,                       -- full_time|part_time|contract|internship|temporary|…
  category          TEXT,
  seniority         TEXT,
  skills            TEXT,                       -- JSON array
  city              TEXT,
  region            TEXT,
  country_code      TEXT,
  lat               REAL,
  lon               REAL,
  remote_mode       TEXT,                       -- onsite|hybrid|remote
  salary_min        REAL,
  salary_max        REAL,
  salary_currency   TEXT,
  salary_period     TEXT,                       -- year|month|week|day|hour
  salary_usd_annual REAL,
  posted_at         INTEGER,
  description       TEXT,                       -- plain text
  source            TEXT NOT NULL,              -- ats:<vendor> | jsonld
  first_seen        INTEGER NOT NULL,
  last_seen         INTEGER NOT NULL,
  closed_at         INTEGER
);
CREATE INDEX jobs_geo ON jobs(lat, lon);
CREATE INDEX jobs_domain ON jobs(domain_id);
CREATE INDEX jobs_board ON jobs(board_key, closed_at);
