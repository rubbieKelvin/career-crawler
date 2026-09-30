# 12 — CV profile: steering relevance and "reputable" scope

**Decision:** the user can provide a CV (**PDF or text/Markdown only**). It becomes a structured **Profile**, and the profile steers:
1. which jobs count as relevant (ranking),
2. which companies/domains are in scope, and
3. what the crawler explores next.

The LLM builds the profile when it's enabled; a deterministic parser is the fallback. Without a CV everything still works, and relevance is simply neutral.

## Input
- Crawler: `--cv path/to/cv.pdf`. UI: `POST /api/profile/cv` (multipart upload), plus the profile editor.
- Accepted: `.pdf`, `.md`, `.markdown`, `.txt`. **Validate content, not just the extension**:
  - a PDF must start with the `%PDF-` magic bytes
  - text files must be valid UTF-8
  - size cap ~5 MB
  - anything else is rejected with a clear error
- PDF text extraction: `pdf-extract` (or `lopdf` directly). If the PDF has no text layer (a scanned image), fail with *"no extractable text; please upload a text-based PDF or a .md version"*. OCR is out of scope.
- The CV stays local (the app is local-only). Store the extracted text + its hash in the DB. Don't copy the original file into the repo; keep data files under a git-ignored `data/` directory.
- Privacy: when the LLM is enabled, CV text is sent to the provider (DeepSeek). Say so in the UI/CLI the first time, and allow `llm.send_cv = false`, which forces the parser path for the CV while other tasks still use the LLM.

## Profile model
```rust
struct Profile {
    titles: Vec<String>,            // "Backend Engineer", "Software Engineer"
    seniority: Option<Seniority>,   // intern..principal, from titles + years
    years_experience: Option<f32>,
    skills: Vec<WeightedSkill>,     // { name, weight }; weight = recency × frequency
    industries: Vec<String>,        // fintech, e-commerce, …
    locations: Vec<Place>,          // current + preferred (geocoded via GeoNames)
    remote: RemotePref,             // onsite | hybrid | remote | any
    relocate: bool,
    salary_expectation: Option<Money>,
    languages: Vec<String>,
    // user-editable; never overwritten by a re-extraction
    must_have: Vec<String>, exclude: Vec<String>,
    excluded_companies: Vec<String>,
}
```
- The **LLM path** uses `prompts/cv_profile.v1.md` and returns JSON matching this schema (see `10-llm.md`, cached by CV hash).
- The **parser fallback** has no LLM:
  - split sections by common headings (Experience, Work History, Skills, Education, Summary)
  - match skills against a curated taxonomy file `data/skills.toml` (name + aliases: "JS" → JavaScript, "k8s" → Kubernetes)
  - match titles against `data/titles.toml`
  - get years of experience from date-range regexes (`Jan 2020 – Present`)
  - find locations by matching city names against GeoNames
- The UI shows the profile as **editable chips**. User edits win and are stored separately from the extracted fields, so a re-extraction doesn't wipe them.

## 1. Job relevance (decision: store all jobs, rank by match)
Storing every job is cheap, and it means changing the CV doesn't require a re-crawl. Relevance is a **score**, not a filter.

Each (profile, job) pair gets a `match_score ∈ [0,1]` plus a list of reasons:
| Component | How |
|---|---|
| Skills | weighted overlap between profile skills and job skills/description (FTS5/BM25 on the taxonomy aliases) |
| Title / category | taxonomy match with the profile's titles; a neighboring category gets partial credit |
| Seniority fit | penalize gaps of 2+ levels in either direction |
| Location / remote | distance to the profile's locations, or remote-eligible for the profile's country |
| Salary | above the expectation = a bonus, well below = a penalty, unknown = neutral |
| Hard rules | `exclude` terms/companies → 0; missing `must_have` → capped |

- Computed deterministically at job ingest, for the active profile.
- **LLM rerank** (optional) runs only on the top ~50 new jobs per day, returning `{fit: 0-10, why}`. That keeps cost bounded and makes "why this job" explanations possible.
- NL search (`10-llm.md`) uses the profile as defaults ("jobs for me") and sorts by `match_score` unless the query says otherwise.

## 2. "Reputable" scope
Two layers:
- A **reputability floor** that applies to everyone, independent of the CV: the heuristics in `02-career-page-detection.md`. It filters out parked domains, spam and aggregators.
- A **profile scope** that ranks companies rather than excluding them:
  - industry match with `profile.industries`
  - company located in / hiring in `profile.locations` (or remote-friendly)
  - has matching job categories (known after harvesting)
  - `excluded_companies` → blocked

## 3. Crawl steering
Add profile terms to link/domain scoring (`01-crawl-strategy.md`):
- Boost discovery links from pages about matching industries/regions (e.g. "Lagos fintech startups", "Nigerian tech companies").
- **Feedback loop**: once a domain yields high-match jobs, boost its outgoing links and the seed/source page that led to it. This rewards "neighborhoods" of the web that produce relevant jobs. It's a simple bandit: keep a `yield` per source domain.
- On careers pages, prefer department links matching profile categories (`/careers/engineering` over `/careers/sales`) when a board is paginated or split by department.
- **Seed suggestions** (LLM, optional): propose search phrases / directory types for the profile. The user approves them in the UI before they become seeds. The LLM never adds URLs to the frontier on its own.

## Profile changes
Changing the active profile triggers these background jobs:
1. recompute `job_matches` for existing jobs, in batches
2. re-score the queued part of the frontier
3. emit a `profile_changed` event so the UI refreshes its rankings

## Storage (additions to 04)
```sql
CREATE TABLE profiles (
  id INTEGER PRIMARY KEY, name TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 0,
  cv_hash TEXT, cv_text TEXT, source TEXT NOT NULL,   -- llm|parser
  extracted TEXT NOT NULL,                            -- JSON Profile from the CV
  overrides TEXT NOT NULL DEFAULT '{}',               -- JSON user edits (win on merge)
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE TABLE job_matches (
  profile_id INTEGER NOT NULL REFERENCES profiles(id),
  job_id     INTEGER NOT NULL REFERENCES jobs(id),
  score REAL NOT NULL, reasons TEXT, llm_fit REAL, llm_why TEXT,
  computed_at INTEGER NOT NULL,
  PRIMARY KEY (profile_id, job_id)
);
CREATE INDEX job_matches_rank ON job_matches(profile_id, score DESC);
```
Multiple profiles are supported, one active at a time.

## Process ownership
- The UI accepts the upload and writes the `profiles` row, then sends `control_commands: profile_changed`.
- The crawler does the heavy parts: extraction, when the CV arrives via the UI, and recomputing matches and the frontier.
- CLI `--cv` does the same thing from the crawler side.
