# 06 — Politeness, safety, crawler traps

## Politeness
- Honor `robots.txt` (cache per host, TTL ~24h), including `Crawl-delay`.
- Per-host: ≤1–2 concurrent requests, min delay ~1s (configurable). Global concurrency ~32–64.
- Identifiable User-Agent with contact URL: `career-crawler/0.1 (+https://…)`.
- Back off on 429/503, honor `Retry-After`. Exponential backoff per host; mark host blocked after repeated failures.
- Prefer ATS APIs over scraping ATS HTML — lighter for everyone.

## Limits
- Max body size (e.g. 5 MB), request timeout (~15s), max redirects (5).
- Only process `text/html`, `application/xhtml+xml`, `application/json`, `application/ld+json`, sitemap XML.

## Traps
- Infinite calendars / pagination / faceted search: cap depth per domain, cap pages per domain, detect repeating path segments, cap query-param variants per path.
- Session IDs in URLs: strip known params.
- Soft 404s: content-hash duplicates across URLs on same host → stop.

## Legal / ethical
- Respect ToS where scraping is explicitly disallowed (LinkedIn, Indeed, Glassdoor): blocklist job aggregators; we want *primary* company sources anyway.
- Store only public job data; no personal data collection.
