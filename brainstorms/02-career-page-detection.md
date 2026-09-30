# 02 — Reputable site detection & finding the careers page

## "Is this a reputable company site?"
Score the **domain** from its homepage (+ maybe one about page). Signals:
- Has `Organization` / `Corporation` schema.org JSON-LD, or OpenGraph `og:site_name`.
- Footer contains ©, company-ish words (Inc, Ltd, GmbH, LLC), privacy/terms links.
- Has about/team/contact/careers nav links.
- HTTPS, valid cert, reasonable page weight, not parked (`domain for sale`, registrar landing pages).
- Not in a blocklist of platforms (social networks, wikis, news aggregators, blog hosts, app stores, search engines).
- Optional: Tranco rank as a prior; inbound-link count from our own crawl graph (PageRank-lite).

Output: `company_score ∈ [0,1]` + reasons stored for debugging. Threshold → move domain into harvest mode.

## Finding the careers link
Ordered attempts, stop at first success:
1. **Nav/footer anchors** matching careers vocabulary (multi-language: `careers, jobs, join us, we're hiring, karriere, emplois, empleo, vacatures, lavora con noi`).
2. **Known ATS links** anywhere on the page (often the careers link *is* an ATS URL).
3. **Well-known paths** probe (HEAD/GET, cheap): `/careers`, `/jobs`, `/careers/`, `/company/careers`, `/about/careers`, `/join`, `/work-with-us`. Also subdomains `careers.<domain>`, `jobs.<domain>`.
4. **sitemap.xml** scan for career-ish URLs.

## Careers page → job list
A careers page is often a landing page linking to an ATS or embedding one (iframe/script from greenhouse/lever/ashby). Detect embed scripts (`boards.greenhouse.io/embed`, `jobs.lever.co`, `ashbyhq.com/...embed`) and extract the board token → go straight to the ATS API (see 03).

## JS-rendered careers pages → headless browser (decided)
Many careers pages render their jobs client-side, so we use a headless Chromium through **`chromiumoxide`** (CDP, tokio-native).

**On/off switch at two levels:**
- Cargo feature `headless` (the default build doesn't need Chrome at all)
- Runtime flag `--headless on|off` / config `browser.enabled`, which can also be toggled from the UI via `control_commands`

**When to render** (fetch with plain HTTP first, always):
- The page scored as careers but yielded 0 jobs from ATS, JSON-LD and HTML heuristics, **or**
- It looks like an SPA shell: tiny visible text, `<div id="root|app|__next">`, lots of JS bundles, `<noscript>` "enable JavaScript", **or**
- The LLM careers-page check says `is_js_rendered` (see `10-llm.md`).

**How:**
- A browser pool with a small tab limit (2–4 tabs; config `browser.max_tabs`), one Chrome process, and a restart after N pages to limit memory leaks.
- Wait for network idle or a job-list selector, with a hard timeout (~20s). Block images, fonts and media to save bandwidth.
- **Intercept network responses (CDP `Network.responseReceived`)**. SPA careers pages usually fetch a JSON jobs API. Once that request is spotted, record the endpoint on the domain (`domains.jobs_api_url`) and call it directly with plain HTTP next time, with no browser needed.
- Record the rendered page in `pages` with `rendered = 1`.
- Politeness still applies: the same per-host limiter and robots rules.

**Metrics:** browser child-process RSS/CPU, pages rendered, render latency, and bytes via CDP `Network.loadingFinished.encodedDataLength` (see `09-metrics.md`).
