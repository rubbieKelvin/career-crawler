# 08 — Decisions & open questions

## Decided (2026-09-30)
| Question | Decision | See |
|---|---|---|
| LLM assist? | **Yes**, for classification/extraction accuracy (gray-zone only) and for natural-language job search. DeepSeek first, behind an OpenAI-compatible provider abstraction so we can migrate. | `10-llm.md` |
| Headless browser? | **Yes**, for JS-heavy careers pages, behind an on/off flag. | `02-career-page-detection.md` |
| Single binary vs split? | **Split** crawler and UI processes, sharing SQLite. | `11-process-architecture.md` |
| Graph granularity? | Domain-level by default, **drill into a page-level subgraph per domain**. | `05-realtime-visualization.md` |
| Deployment? | **Local only.** UI binds 127.0.0.1, no auth. | `11-process-architecture.md` |

## Still open
- **Scope of "reputable"**: any company, or a filter (size, industry, country)? This affects scoring and seeds. Tied to this: should seeds lean toward particular regions (e.g. Nigeria/Africa, given the Lagos example)?
- **Job relevance**: store all jobs, or filter? A user profile (preferred roles/locations) could steer the crawl's scoring, not just the search.
- **Re-crawl policy**: how often to refresh known boards (daily?) vs. discover new domains?
- **LLM budget**: a daily token/cost cap for DeepSeek, and which tasks are allowed to spend it.
- **Salary scarcity**: many postings have no salary. Should "nice paying" also use estimates (company tier, role, seniority), or only posted salaries?
