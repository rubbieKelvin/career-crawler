# 08 — Open questions

- **Scope of "reputable"**: any company, or filter (size, industry, country, tech)? Affects scoring and seeds.
- **Job relevance**: store all jobs, or filter by keywords/role/location (e.g. remote software roles)? A filter could also steer the crawl.
- **LLM assist?** Use an LLM to classify ambiguous domains / extract jobs from messy HTML. Costly but high-accuracy; could be an optional fallback tier.
- **Headless browser** for JS-heavy careers pages — worth the complexity/weight?
- **Re-crawl policy**: how often to refresh known boards (daily?) vs discover new domains?
- **Single binary** (crawler + UI in one process) vs split crawler/UI processes sharing the DB? Starting with one binary.
- **Graph granularity**: domain-level only, or allow drilling into page-level subgraphs per domain?
- **Deployment**: local-only, or eventually run on a server with auth on the UI?
