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
2. The LLM (tool call) returns a `JobQuery`:
   ```json
   { "keywords": [], "categories": ["engineering","data","product","design"],
     "industries": ["technology"],
     "near": { "place": "Lagos", "radius_km": 50 }, "include_remote": "region",
     "salary": { "mode": "top_percentile", "value": 0.25 },
     "posted_within_days": 60, "sort": "salary_desc", "limit": 50,
     "explanation": "Tech roles within 50km of Lagos (plus remote roles open to Nigeria), top 25% pay for that area" }
   ```
3. Rust resolves this into a query: `near.place` is geocoded to lat/lon with GeoNames, which gives a bounding box plus a haversine filter. "Nice paying" becomes **relative**: the top percentile of `salary_usd_annual` among jobs matching the same location/category. An absolute number would be meaningless across markets.
4. Parameterized SQL + FTS5 on title/description/skills handles the keywords.
5. The UI shows results **and the interpreted filter as editable chips**, so the user can see and fix what the LLM understood.

Why not text-to-SQL: the SQL would be unsafe (even on a read-only connection it can run expensive queries), hard to validate, and it can't do geocoding or percentile logic properly. As a power-user/debug mode we could later allow text-to-SQL on a separate **read-only** connection (`PRAGMA query_only = ON`) with a timeout.

Later: semantic search with **local embeddings** (`fastembed-rs`) + `sqlite-vec`, so query quality doesn't depend on the chat provider. DeepSeek doesn't offer an embeddings API (verify).

## Where NL search lives
In the **UI process** (see `11-process-architecture.md`), at route `POST /api/search/nl`. The UI process uses the same `llm` crate as the crawler.
