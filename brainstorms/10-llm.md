# 10 — LLM: classification, extraction, natural-language search

**Decision:** use an LLM wherever heuristics are unsure. Start with **DeepSeek**, but keep the provider swappable.

## Provider abstraction
DeepSeek's API is OpenAI-compatible (chat completions, JSON output, tool calls). So build **one OpenAI-compatible client** with configurable `base_url`, `model` and API key. That also covers OpenAI, Anthropic's OpenAI-compatible endpoint, Groq, OpenRouter, and local Ollama/llama.cpp servers. Put it behind a trait anyway, for providers that don't fit:

```rust
#[async_trait]
trait LlmProvider {
    async fn complete_json<T: DeserializeOwned + JsonSchema>(&self, task: &LlmTask) -> Result<LlmResult<T>>;
}
struct LlmResult<T> { value: T, tokens_in: u32, tokens_out: u32, cached: bool, latency_ms: u32 }
```

```toml
[llm]
enabled  = true
provider = "openai_compatible"
base_url = "https://api.deepseek.com"
model    = "deepseek-chat"          # verify current model names when implementing
api_key_env = "DEEPSEEK_API_KEY"
max_concurrency = 4
daily_token_budget = 2_000_000
```

Rules:
- **Structured output only.** Every call returns JSON, parsed into a serde struct (`schemars` generates the schema for the prompt). If parsing fails: retry once with the error attached, then fall back to the heuristic result.
- **Cache by `(task, prompt_version, model, content_hash)`** in an `llm_calls` table, so re-crawls and restarts don't pay twice. The same table is the audit log: input hash, output, tokens, cost, latency.
- **Prompts are versioned files** (`prompts/<task>.v1.md`). Bumping the version invalidates the cache for that task only.
- **Metrics**: tokens in/out, estimated cost, calls, cache hit rate and error rate go into `09-metrics.md`, and there is a token budget like the byte budget.
- Keep the input small: send extracted text/structure (title, nav links, footer, visible text truncated to N tokens), never raw HTML.
- The crawler must work with `llm.enabled = false`. The LLM is an accuracy upgrade, not a dependency.

## Where the crawler uses it (only when the heuristics are unsure)
| Task | When | Output |
|---|---|---|
| **Domain classification** | heuristic `company_score` falls in a gray zone (e.g. 0.3–0.7) | `{is_company, company_name, industry, hq_country, confidence, reason}` |
| **Careers-link selection** | several candidate links, or none match the vocabulary | `{careers_url \| null, confidence}` |
| **Careers-page check** | a page we think is careers yielded 0 jobs | `{is_careers_page, is_js_rendered, ats_hint}` → may trigger the headless browser |
| **Job extraction** | HTML-heuristic tier (no ATS, no JSON-LD) | `Vec<Job>` |
| **Job enrichment** | every new job, in batches | normalized fields, see below |
| **CV → Profile** | a CV is uploaded/changed (cached by hash; skipped if `llm.send_cv = false`) | `Profile` (see `12-cv-profile.md`) |
| **Match rerank** | top ~50 new jobs/day for the active profile | `{fit, why}` |

Cheap heuristics always run first. The LLM sees only ambiguous cases, which keeps cost bounded.

## Enrichment at ingest (this is what makes NL search work)
NL queries like *"nice paying jobs in tech around Lagos"* only work if jobs have normalized fields. At ingest, enrich each job (deterministic parsing first, the LLM for the rest):
- `category` (engineering, design, sales, …), `industry` (from its domain), `seniority`
- `city`, `region`, `country_code`, `lat`, `lon`. Geocode through an **offline GeoNames cities dataset** (no network, deterministic). The LLM only splits messy location strings like "Lagos / Remote (EMEA)".
- `remote`: onsite / hybrid / remote, plus the allowed regions for remote
- `salary_min/max` normalized to **annual, in the original currency**, plus `salary_usd_annual`. FX rates come from a small table refreshed occasionally.
- `skills` (tags) for keyword matching

## Natural-language query
**Approach: NL → structured filter → parameterized SQL.** No free-form text-to-SQL.

1. The user types *"i'm looking for nice paying jobs in tech around Lagos"*.
2. The LLM (one JSON answer, not a tool call) returns a `JobQuery`:
   ```json
   { "keywords": ["backend"], "categories": ["engineering","data","product","design"],
     "industries": ["technology"],
     "near": { "place": "Lagos", "radius_km": 50 }, "remote": "any",
     "salary": { "mode": "top_percentile", "value": 0.25 },
     "posted_within_days": 60, "sort": "salary_desc", "limit": 50,
     "explanation": "Tech roles within 50km of Lagos (plus remote roles open to Nigeria), top 25% pay for that area" }
   ```
   "Plus remote roles open to Nigeria" is not a field: remote jobs open to the place's country count as near it (see `search::run`).
3. Rust resolves this into a query: `near.place` is geocoded to lat/lon with GeoNames, which gives a bounding box plus a haversine filter. "Nice paying" becomes **relative**: the top percentile of `salary_usd_annual` among jobs matching the same location/category. An absolute number would be meaningless across markets.
4. Parameterized SQL + FTS5 on title/description/skills handles the keywords.
5. The UI shows results **and the interpreted filter as editable chips**, so the user can see and fix what the LLM understood.
6. What the search had to decide for itself (the salary threshold it used, a place it couldn't place, the order it fell back to) comes back as notes next to the chips. What was built is listed under *Implemented (milestone 12)* at the end of this file.

Why not text-to-SQL: the SQL would be unsafe (even on a read-only connection it can run expensive queries), hard to validate, and it can't do geocoding or percentile logic properly. As a power-user/debug mode we could later allow text-to-SQL on a separate **read-only** connection (`PRAGMA query_only = ON`) with a timeout.

Later: semantic search with **local embeddings** (`fastembed-rs`) + `sqlite-vec`, so query quality doesn't depend on the chat provider. DeepSeek doesn't offer an embeddings API (verify).

## Where NL search lives
In the **UI process** (see `11-process-architecture.md`), at route `POST /api/search/nl`. The UI process uses the same `llm` crate as the crawler.

## Implemented (milestone 10)
What exists, and where it differs from the plan above:
- **`career-llm`**: `Provider` trait (boxed futures, so it stays object-safe without `async_trait`) with one `OpenAiCompatible` implementation (JSON mode, `temperature 0`). `Llm::complete_json::<T>(prompt, input)` adds the cache, a rolling-24h token budget counted from `llm_calls`, a concurrency semaphore, backoff retries for 429/5xx/network errors, and the one-retry-with-the-parse-error rule. `llm::testing::FakeProvider` is the scripted provider the tests use.
- **Config** `[llm]`: **`enabled` defaults to `false`** (it sends page text to a third party and costs money), and no key in the environment means "continue without the LLM" with a warning, not an error. There is no `provider` key yet, since there is only one implementation. Added `request_timeout_secs`, `input_price_per_mtok` / `output_price_per_mtok` (cost estimate only) and `enrich_batch_size`.
- **No `schemars`**: each prompt (`crates/llm/prompts/<task>.v1.md`) spells out its JSON shape by hand, and the serde structs in `tasks.rs` are the parsers. Output is validated field by field afterwards (categories against the allowed lists, country codes against the country table, coordinates only from our own gazetteer), since well-formed JSON can still contain nonsense.
- **`llm_calls`** (migration 0008) holds every provider call, ok or not, with tokens, estimated cost and latency. Only `ok` rows are cache hits. Cache hits are counted in metrics but not logged as rows.
- **Domain classification**: when a *conclusive main homepage* leaves a domain `probing` (score in the 0.3–0.6 gray zone) and it hasn't been asked about (`domains.llm_checked_at`), `store::record` returns a `ClassifyRequest`. `crawl::process` asks the LLM after the transaction, and `store::record_verdict` applies the answer: `confidence >= 0.7` settles `company` / `not_company` exactly like the heuristics (budgets, queued and deferred URLs, and for a company the careers probes and sitemap scan its homepage would have triggered); anything less only keeps industry and country. A failed call is not recorded as "asked". The input is a text digest (title, site name, heuristic score and signals, link texts, footer, the first 2,500 characters of text). Not done: a backlog pass for domains that were gray before the LLM was enabled (their homepage would need refetching).
- **Job enrichment** (`career_core::enrich`, `crawler/src/enricher.rs`):
  - Rules always run first: category and seniority from title keywords, skills from a keyword list, an offline city table (`crates/core/data/cities.tsv`, ~145 job markets, the same shape as GeoNames `cities15000`, so the real dump can replace it), remote mode and regions from the location text, salary to annual and to USD through a static FX table.
  - The LLM is asked, in batches, only for jobs the rules couldn't finish (no category, or a location they couldn't place). Rules win where they have a value, and the LLM's skills and remote regions are added to theirs.
  - `jobs.enrich_state` is `pending`, then `rules` (waiting for the LLM, at most 3 tries), then `done`. A re-crawl of an unchanged posting keeps its enrichment, and a changed one goes back to `pending`.
  - Raw `salary_*` stay as the source gave them; the normalized values are `salary_annual_min/max` and `salary_usd_annual`.
  - It runs as a background task in the crawler and as `crawler enrich [--no-llm]`, which backfills a database.
- **Not done** from the task table: careers-link selection, the careers-page check, LLM job extraction, the CV and rerank tasks. Job extraction belongs with the HTML heuristic tier in milestone 14, and the CV tasks with milestone 11.

## Implemented (milestone 12): natural-language search
Built as planned — the LLM reads the words into a filter set, Rust builds the SQL — with three deviations forced by the storage engine and one by the plan's own open question:
- **`career_core::search`** owns the model and the SQL. `JobQuery` is the closed filter set (keywords, categories, industries, `near {place, radius_km}`, remote, `salary {mode, value}`, `posted_within_days`, `sort`, `limit`, `explanation`); `search::run` turns it into one parameterized statement whose every user value is bound. `JobQuery::sanitized` drops unknown values, clamps numbers and says what it ignored (the UI shows that as notes), so a model's nonsense costs an odd result page, never a failed search.
- **`jobs_fts`** (migration 0010): an FTS5 index over `jobs(title, description, skills)`, external content plus triggers (and a `rebuild` for rows that existed). Keywords match any term, quoted so punctuation can't become FTS syntax; `bm25` with the title weighted 8× ranks them. The corpus is small enough that the index is rebuilt in place as jobs change — there is no separate sync job to go stale.
- **Geo**: this SQLite build has no trigonometry (`SQLITE_ENABLE_MATH_FUNCTIONS` is off in the bundled library), so the radius filter is a two-step: a bounding box in SQL (`enrich::geo::bbox`) and an exact haversine in Rust (`enrich::geo::distance_km`, shared with `matching`). A bbox-only search is capped at 2 000 candidates and says so when the cap bites. Jobs the enricher couldn't place (no coordinates) pass on their country, which is all the record says.
- **"Well paid" is relative, over posted salaries only** — the open question in `08-open-questions.md` stays open for jobs with no posted salary: they are excluded from a salary-filtered search (and from the percentile itself) rather than guessed at. `top_percentile` resolves a threshold against the *matching* jobs (count, then the value at that rank) and the note says "the top 25% of the 1 220 matching jobs that posted a salary is $222 000/yr or more", so the number is auditable.
- **Route**: `POST /api/search/nl` takes either `{"query": "…"}` (read by the LLM, or by a keyword fallback when the LLM is off or the call fails — the words minus stopwords, and the note says which path was taken) or `{"filters": {…}}` (the chips, no LLM). No new config key: NL search rides on `[llm] enabled` (its text is not as private as a CV, which keeps its own `send_cv` gate), and the provider host is echoed as `llm.host` so the UI can say where the words went.
- **The profile** (milestone 11) is passed as one context line in the digest and, when one is active, is the default order (`sort: "match"`, hits carry `match_score`) — unless the query asks for another order.
- **UI**: the Search tab (`static/search.js`) shows the reading as chips — remove any of them, add keywords or industries, change the radius, the work style or the order — and every edit re-posts `filters`, so the result set is exactly what the chips say. Notes above the results explain the percentile, a place it couldn't resolve, or a missing CV.
- **Not done**: no text-to-SQL debug mode, no embeddings/semantic search, no salary estimates for postings that don't post one, and the LLM does not search for keywords by itself beyond the 5 it returns.
