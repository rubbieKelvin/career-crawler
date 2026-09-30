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

## JS-rendered careers pages
Many careers pages render jobs client-side. v1: rely on ATS detection + JSON-LD. v2 option: headless browser (`chromiumoxide` / `fantoccini`) behind a feature flag, only for pages that scored as careers but yielded 0 jobs.
