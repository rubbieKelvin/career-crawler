# career

A recursive web crawler that finds **company career pages and the job postings on them**, stores everything in a local SQLite database, and serves a live web UI to watch and explore the crawl.

- **Prioritized frontier**, not a blind BFS: links are scored (careers words, ATS boards, depth, saturation, blocklists) and each host is crawled politely.
- **Company detection**: pages are scored for "looks like a company" (JSON-LD org, legal suffix, ©, careers/about/privacy links). Only company-like domains get a deeper crawl.
- **Careers detection and job extraction**: careers landing pages, `/careers` and `/jobs` probes, sitemaps, and direct API harvesting for Greenhouse, Lever and Ashby boards, plus JSON-LD `JobPosting` on any page.
- **Enrichment**: category, seniority, skills, city/country, remote mode and USD-normalized salary, with deterministic rules first and an optional LLM tier for what the rules can't finish.
- **Live UI**: network graph of the crawl, event feed, metrics charts, history replay, and per-domain page drill-down.

## Architecture

Two processes share one SQLite database (WAL mode), local only:

```
crawler ──writes──▶ SQLite (events, pages, jobs, …) ◀──tails── ui ──▶ browser (WS + REST)
   ▲                                                    │
   └──────────── control_commands (pause/resume/…) ◀────┘
```

The crawler is the only writer of crawl data. The UI tails the `events` table and writes only to `control_commands` and its own tables, so it keeps working while the crawler is stopped. Design notes live in [`brainstorms/`](brainstorms/); start with [`00-overview.md`](brainstorms/00-overview.md).

Workspace crates (`crates/`):

| Crate | Package | Role |
| --- | --- | --- |
| `core` | `career-core` | config, DB and migrations, events, seeds, URL handling, frontier storage, job model, enrichment rules |
| `llm` | `career-llm` | provider trait, OpenAI-compatible client (DeepSeek by default), cache, audit log, token budget |
| `crawler` | `career-crawler` | fetcher, robots, politeness, parser, classifier, careers detection, extractors, scheduler (binary: `crawler`) |
| `ui` | `career-ui` | axum server, live tailer, REST API, static frontend (binary: `ui`) |

## Getting started

Requirements: a recent stable Rust toolchain. [`just`](https://github.com/casey/just) is optional but convenient.

```bash
# 1. Optional: copy and edit the config (every key is optional)
cp config.example.toml config.toml

# 2. Put some starting URLs in seeds.txt (portfolios and company directories work best)

# 3. Run the crawler
cargo run -p career-crawler -- --seeds seeds.txt --max-pages 500

# 4. In another terminal, start the UI and open http://127.0.0.1:7878
cargo run -p career-ui
```

The database defaults to `data/career.db` (git-ignored). Config is read from `--config`, else `./config.toml` if it exists, else defaults. Unknown config keys are rejected. Set `RUST_LOG` to change log verbosity (default `info`).

### Useful commands

```bash
cargo run -p career-crawler -- fetch <url>       # debug: one robots-aware fetch + parse, no DB
cargo run -p career-crawler -- enrich [--no-llm] # enrich stored jobs and exit
cargo run -p career-ui -- --bind 127.0.0.1 --port 7878
```

With `just`:

```bash
just crawl --seeds seeds.txt   # run the crawler
just ui                        # run the UI
just test [filter]             # run tests
just check                     # fmt-check + clippy + tests
just events [n]                # recent events from the DB
just sql                       # open the DB in sqlite3
```

## Optional LLM tier

Off by default. Enable `[llm]` in `config.toml` and set the API key environment variable named there (`api_key_env`). The LLM settles gray-zone company classifications and fills job fields the rules couldn't. Without a key, or on any error, the crawler falls back to heuristics. See [`brainstorms/10-llm.md`](brainstorms/10-llm.md).

## Development

- Schema changes go in a new file `crates/core/migrations/NNNN_name.sql`; never edit an applied migration.
- Tests never hit real sites, ATS APIs or LLM providers (wiremock, inline fixtures, `FakeProvider`).
- Run `just check` before committing. It does not rebuild `target/debug/crawler`, so run `cargo build` before manual runs.

## Status

Milestones 1–10 are done (crawl pipeline, company classification, careers detection, job extraction, live UI and graph, history replay, LLM layer and enrichment). Planned next: CV profile matching and natural-language job search.

## Crawling politely

The crawler honors `robots.txt` (including `Crawl-delay`), allows one in-flight request per host with a minimum gap, and identifies itself via `user_agent`. If you crawl a lot, add a contact URL to it in your config.
