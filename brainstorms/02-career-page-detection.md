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

**Implemented (milestone 4, `crawler/src/classify.rs`).** Every non-duplicate, non-ATS-board page is scored, and the domain keeps its **best** page score:
- Weights: Organization JSON-LD .25, careers link (internal or its ATS board) .2, legal suffix in footer (Ltd/Inc/GmbH…) .2, © .1, about .1, privacy .1, and .05 each for contact, terms, `og:site_name`, HTTPS and 2+ inbound domains (.1 for 5+).
- Status:
  - `company` at ≥ 0.6 (never downgraded)
  - `not_company` below 0.3, but only once a **conclusive** main homepage (`domain/` or `www.domain/`) has been seen, or immediately if parked
  - otherwise `probing`
- **Thin pages (JS shells) are inconclusive, not negative.** anduril.com's homepage is a Next.js shell with 0 links but valid Organization JSON-LD, so it must not become `not_company`. These are the headless-browser candidates.
- Name preference: a JSON-LD org whose `url` is this domain, then `og:site_name`, then any org. Articles embed other organizations (engadget.com was once named "University of Kent"). The homepage's name overrides an earlier deep page's.
- If a new domain's first page isn't the homepage, the homepage is queued as `probe_home` (score 60) so the domain gets classified.
- On a status change: `DomainClassified` event; queued scores on the domain shift (+10 company, −15 not_company, undone if it leaves not_company); `deferred` URLs are revived when it becomes a company.

Observed on an 80-page crawl from the default seeds (2026-09-30): 14 companies (Stripe, Flutterwave, a16z, Paystack, Databricks, Okta, Sequoia, YC, …), 2 not-companies, and 7 `probing` in the 0.35–0.4 gray zone (blogs, event sites, startupschool.org). Those are the cases the LLM (milestone 10) should settle.

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
- It looks like an SPA shell: tiny visible text, `<div id="root|app|__next">`, lots of JS bundles, `<noscript>` "enable JavaScript". The classifier already flags these as `thin` / `conclusive = false` (milestone 4). **Or**
- The LLM careers-page check says `is_js_rendered` (see `10-llm.md`).

**How:**
- A browser pool with a small tab limit (2–4 tabs; config `browser.max_tabs`), one Chrome process, and a restart after N pages to limit memory leaks.
- Wait for network idle or a job-list selector, with a hard timeout (~20s). Block images, fonts and media to save bandwidth.
- **Intercept network responses (CDP `Network.responseReceived`)**. SPA careers pages usually fetch a JSON jobs API. Once that request is spotted, record the endpoint on the domain (`domains.jobs_api_url`) and call it directly with plain HTTP next time, with no browser needed.
- Record the rendered page in `pages` with `rendered = 1`.
- Politeness still applies: the same per-host limiter and robots rules.

**Metrics:** browser child-process RSS/CPU, pages rendered, render latency, and bytes via CDP `Network.loadingFinished.encodedDataLength` (see `09-metrics.md`).
