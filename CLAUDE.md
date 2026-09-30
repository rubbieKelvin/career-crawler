# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

Milestones 1–5 (workspace skeleton; fetch + parse; frontier + end-to-end crawl; company classification + budgets; careers detection) are done; see `brainstorms/07-milestones.md` for what's next. Design notes live in `brainstorms/` (numbered `NN-topic.md`). Read `brainstorms/00-overview.md` first; the notes are the source of truth for intent where code doesn't exist yet. When a brainstorm decision is implemented or overturned, update the brainstorm rather than letting it drift.

## What this program is

A recursive web crawler whose goal is finding **company career pages and the job postings on them**, storing results in a local SQLite database. It must:

1. Decide *which* sites are worth crawling (prioritized frontier, not a blind BFS).
2. On a site judged reputable/company-like, locate its careers/jobs link and crawl that section.
3. Extract job postings and store them with normalized fields (location, salary, category).
4. Serve a web UI that shows the live crawl and the crawl history as a network graph, updated in real time, plus metrics and natural-language job search (e.g. "nice paying jobs in tech around Lagos").

## Commands

A `Justfile` wraps the common commands (`just` lists them): `just crawl …`, `just ui …`, `just test [filter]`, `just test-crate core [filter]`, `just check` (fmt-check + clippy + tests), and `just events [n]` / `just sql` to inspect the DB, and `just crawl fetch <url>` to debug one page. The recipes assume `data/career.db`; override with `just db=path <recipe>`. The raw cargo equivalents:

```bash
cargo build
cargo run -p career-crawler -- [--config config.toml] [--db path] [--seeds seeds.txt] [--max-pages N]
cargo run -p career-crawler -- fetch <url> [--links N]   # debug: one robots-aware fetch + parse, no DB
cargo run -p career-ui -- [--config config.toml] [--db path]
cargo test                                  # all tests
cargo test -p career-core seeds::           # one crate / module / test-name substring
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Config is optional: the binaries use `--config`, else `./config.toml` if present, else defaults. `config.example.toml` documents the keys, and unknown keys are rejected. `RUST_LOG` controls log level (default `info`). The DB defaults to `data/career.db`, which is git-ignored.

Package names are prefixed `career-` because a crate named `core` would shadow Rust's `core`. The binaries are named `crawler` and `ui`.

## Conventions

- **Explicit `return` in functions**: every function that returns a value ends with `return …;`, not a tail expression. This includes `return Ok(());`, `Default` impls and `return match … { … };`. Closures stay idiomatic (`.map(|d| d.as_millis())`). `clippy::needless_return` is allowed workspace-wide in `Cargo.toml` (`[workspace.lints.clippy]`), and every crate opts in with `[lints] workspace = true`, so a new crate must add that too.
- All timestamps are **unix epoch milliseconds** (`career_core::time::now_ms`).
- Schema changes go in a **new** file `crates/core/migrations/NNNN_name.sql`; never edit an applied migration. Migrations are embedded with `sqlx::migrate!` and run by `db::open` in both binaries.
- Queries use sqlx's runtime API (`sqlx::query`, `query_as`, `query_scalar`), not the compile-time `query!` macros, so no `DATABASE_URL` is needed to build.
- New event variants go in `career_core::events::Event` (serde tag `kind`, snake_case). Keep `Event::kind()` in sync; a test checks that.
- DB tests use `db::test_pool()` (a temp-file DB with migrations applied). HTTP tests use `wiremock`. Use `set_body_raw(body, "text/html")` there, because `set_body_string` forces `text/plain` whatever headers you insert. Time-based tests use `#[tokio::test(start_paused = true)]`.
- Metrics counters are `AtomicU64` fields incremented with `fetch_add(n, Relaxed)`.
- `just check` does not rebuild `target/debug/crawler`. Run `cargo build` (or `just crawl …`) before manual runs, or you'll test a stale binary.

## Intended architecture

**Two processes, one SQLite DB (WAL), local only.** The crawler and the UI are separate binaries in one Cargo workspace. They never talk directly; everything goes through the DB (`brainstorms/11-process-architecture.md`):
- **crawler → UI**: the crawler appends to the `events` table, and the UI tails it (`WHERE id > last`, ~200 ms) and fans out over WS/SSE. So live view and history replay share one code path, and the UI works while the crawler is stopped.
- **UI → crawler**: the UI inserts into `control_commands`, and the crawler polls it (pause/resume, headless on/off, add seeds).
- The crawler is the only writer of crawl data, through a single writer task. The UI writes only its own tables.

Crates (`crates/`); `llm` is still an empty stub, and crawler modules marked (planned) don't exist yet:
- **core** (`career-core`): config, `db` (open + migrate, WAL, stats), `events`, `seeds`, `urls` (normalization, registrable domain), `frontier` (queue storage and selection; no scoring).
- **llm**: an OpenAI-compatible chat client (DeepSeek first; `base_url`/`model`/key-env are config, so switching providers is a config change), versioned prompts in `prompts/`, and a response cache in `llm_calls`. Output is always JSON parsed into serde structs, falling back to heuristics on failure. Every LLM path must be optional (`llm.enabled = false` still crawls). See `brainstorms/10-llm.md`.
- **crawler**, whose internal pipeline is `frontier → fetcher → parser → (classifier, extractor) → store + events → frontier`:
  - *crawl* (scheduler): takes the best queued URL **per host** (skipping busy or cooling hosts), runs up to `max_concurrency` visits, enforces per-budget page caps, depth, `--max-pages` and graceful Ctrl-C. The crawler opens the DB with **one connection** (`db::open_with(path, 1)`), which makes it the single writer.
  - *store*: records a whole visit in **one transaction** (domain, page with content hash for dedup, `page_links`, domain `edges`, scored new frontier URLs, frontier state, event), so the UI never sees partial state. Transient failures retry once at half score.
  - *scoring*: pure heuristic link scores (careers words, ATS boards, external bonus, depth/saturation/archive penalties, blocklists). The `reason` column in `frontier` records which signals fired.
  - *ats*: `ats::board(url)` returns `Board { vendor, token, host }` from board and embed URLs; `key()` = `vendor/token`. `crawl::budget_key` makes each board its own budget instead of sharing the vendor's domain.
  - *careers*: careers landing-page detection (`is_careers_page`), ATS attribution guarded against portfolio pages (`attributed_board`), homepage-only probes of `/careers`, `/jobs`, `careers.<domain>`, and a sitemap scan. The sitemap scan does network I/O, so the store returns a `SitemapScan` request and `crawl::process` runs it *after* the transaction. `Visitor::fetch_resource` is the polite (robots + gate + redirects) fetch for non-page resources.
  - *classify*: pure page → company score in [0,1] (JSON-LD org, legal suffix, ©, careers/about/privacy links, …). `store::classify_domain` keeps each domain's **best** page score and derives `DomainStatus`:
    - `company` ≥ 0.6, never downgraded
    - `not_company` < 0.3, only after a *conclusive* main homepage; thin JS shells prove nothing
    - otherwise `probing`
    - Status changes revive `deferred` URLs and shift queued scores.
  - *budgets* (`crawl::Budgets`): discovery budget until a domain is a company, then harvest; ATS boards always harvest. Over-budget URLs are **deferred**, not skipped.
  - *visit* → *robots*, *politeness*, *fetcher*, *parse* (implemented): `Visitor::visit(url)` is the unit of work. It checks robots.txt (cached per origin), waits on `HostGate` (one in-flight request per host, a minimum gap that `Crawl-delay` can raise), fetches, and **follows redirects itself**, re-checking robots and politeness per hop. The reqwest client has redirects and auto-decompression **off**, so metrics see real wire bytes; `fetcher::decompress` handles gzip/deflate/br with a size cap. `parse` decodes charsets (BOM, then header, then meta, then UTF-8) and extracts title, canonical, meta robots and normalized links.
  - *browser*: headless Chromium (`chromiumoxide`), behind cargo feature `headless` **and** a runtime flag. It's used only for careers pages that yield no jobs or look like SPA shells. It captures jobs-API XHRs so later visits can skip the browser.
  - *LLM classification* (planned): the LLM only for gray-zone domains (`probing`, score 0.3–0.6); `domains.score_reasons` holds the heuristic evidence.
  - *extractor* (planned): jobs in tier order: ATS APIs → JSON-LD `JobPosting` → HTML heuristics → LLM. Enrichment adds the normalized geo/salary/category needed for NL search.
  - *metrics*: `Arc<Metrics>` atomics plus a 1s sampler (CPU/RSS/heap/bytes/LLM tokens) → `metrics_samples` + `metrics_tick` events. It enforces budgets. See `brainstorms/09-metrics.md`.
- **CV profile** (cross-cutting): an optional CV (PDF or MD/TXT only, validated by content) is turned into a `Profile` by the LLM, or by a parser + skills/titles taxonomy fallback. User edits live in `overrides` and win over re-extraction. The profile drives `job_matches.score` (all jobs are stored; relevance is ranking, not filtering), company scope, and frontier scoring. Changing the profile triggers a background re-score. See `brainstorms/12-cv-profile.md`.
- **ui**: an axum server on 127.0.0.1 with the graph (domain nodes, drilling into a page-level subgraph from `page_links`), metrics panel, and natural-language job search. NL search works like this: the LLM turns the text into a structured `JobQuery` filter, Rust builds parameterized SQL from it (geo radius via offline GeoNames, "well paid" as a relative salary percentile), and FTS5 handles keywords. It deliberately avoids free-form text-to-SQL.
