# 01 — Crawl strategy: deciding what to crawl

## Frontier = priority queue
Each entry: `(score, url, depth, discovered_from, reason)`. Pop highest score whose host is currently allowed (politeness window open). Implementation idea: a `BinaryHeap` per host + a global heap of "next-ready host", or simply persist the frontier in SQLite and `SELECT ... ORDER BY score DESC LIMIT n` with a host-ready filter — simpler and resumable for free.

## Two kinds of crawling
1. **Discovery crawl** — hop across domains looking for *companies*. Wide, shallow per domain.
2. **Harvest crawl** — once a domain is judged a company, dive *within* it to find careers + jobs. Narrow, deep-ish, bounded.

Keep per-domain page budgets: e.g. discovery ≤ 3 pages/domain, harvest ≤ 30 pages/domain.

## Link scoring (heuristic v1)
Score = sum of signals, clamped:
| Signal | Weight idea |
|---|---|
| Anchor/URL contains `careers`, `jobs`, `join-us`, `work-with-us`, `hiring`, `vacancies`, `openings` | +++ (harvest) |
| Link points to a known ATS host (greenhouse.io, lever.co, ashbyhq.com, myworkdayjobs.com, smartrecruiters.com, workable.com, bamboohr.com, recruitee.com, teamtailor.com) | ++++ |
| Link is to a *new* registrable domain from a "company-dense" page (startup directories, "our customers", portfolio pages, "partners") | ++ (discovery) |
| Source domain is itself a confirmed company | + |
| Depth from seed | − per level |
| Domain already has many pages crawled | − (diminishing returns) |
| Social / aggregator / CDN / docs / login / cart / tag / calendar URLs | −−− or blocklist |
| File extensions (pdf, jpg, zip…) | drop |

Later: learn weights from outcomes (did this link lead to a job within N hops?). Log features per link so this is possible.

## Seeds
Good seeds are pages that *list many companies*:
- VC portfolio pages (a16z, Sequoia, YC company directory)
- "Top N companies" lists, Tranco top-sites list (filtered)
- GitHub org pages, Product Hunt, Crunchbase-like public lists
- Manual seed file `seeds.txt`

## URL normalization
Lowercase scheme/host, drop fragment, drop tracking params (`utm_*`, `gclid`, `fbclid`, `ref`), sort remaining query params, strip default ports, resolve relative links, collapse trailing slash consistently. Dedup on normalized form.

## Stopping conditions
Global page budget, wall-clock budget, or frontier exhausted/below score threshold.
