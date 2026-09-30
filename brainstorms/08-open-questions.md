# 08 — Decisions & open questions

## Decided (2026-09-30)
| Question | Decision | See |
|---|---|---|
| LLM assist? | **Yes**, for classification/extraction accuracy (gray-zone only) and for natural-language job search. DeepSeek first, behind an OpenAI-compatible provider abstraction so we can migrate. | `10-llm.md` |
| Headless browser? | **Yes**, for JS-heavy careers pages, behind an on/off flag. | `02-career-page-detection.md` |
| Single binary vs split? | **Split** crawler and UI processes, sharing SQLite. | `11-process-architecture.md` |
| Graph granularity? | Domain-level by default, **drill into a page-level subgraph per domain**. | `05-realtime-visualization.md` |
| Deployment? | **Local only.** UI binds 127.0.0.1, no auth. | `11-process-architecture.md` |
| Scope of "reputable" | A general reputability floor, plus a **CV-derived profile** that ranks companies by industry/location fit. | `12-cv-profile.md` |
| Job relevance | **Store all jobs, rank by a CV match score.** CV is PDF or Markdown/text; the LLM extracts the profile, with a parser fallback. The profile also steers the crawl. | `12-cv-profile.md` |

## Still open
- **Re-crawl policy**: how often to refresh known boards (daily?) vs. discover new domains?
- **LLM budget**: a daily token/cost cap for DeepSeek, and which tasks are allowed to spend it (profile extraction and top-N rerank are the new ones).
- **Salary scarcity**: many postings have no salary. Should "nice paying" also use estimates (company tier, role, seniority), or only posted salaries?
- **Seeds by region**: seed lists for the profile's regions (e.g. Nigeria/Africa) will probably need to be curated by hand at first. Where do we get them from?
- **Match weights**: how to tune the component weights of `match_score`? Could add thumbs up/down on jobs in the UI and learn from that later.
