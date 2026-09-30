# 03 — Job posting extraction

Priority order (most reliable → least):

## 1. ATS public APIs (best)
| ATS | Detect | Endpoint |
|---|---|---|
| Greenhouse | `boards.greenhouse.io/<token>`, `job-boards.greenhouse.io/<token>` | `https://boards-api.greenhouse.io/v1/boards/<token>/jobs?content=true` |
| Lever | `jobs.lever.co/<company>` | `https://api.lever.co/v0/postings/<company>?mode=json` |
| Ashby | `jobs.ashbyhq.com/<org>` | `https://api.ashbyhq.com/posting-api/job-board/<org>` |
| Workable | `apply.workable.com/<co>` | widget JSON API |
| SmartRecruiters | `jobs.smartrecruiters.com/<co>` | `https://api.smartrecruiters.com/v1/companies/<co>/postings` |
| Workday | `<co>.wd*.myworkdayjobs.com/...` | POST `/wday/cxs/<co>/<site>/jobs` |
| Recruitee / Teamtailor / BambooHR | subdomain patterns | public JSON/RSS feeds |

Board identity is implemented (milestone 5, `crawler/src/ats.rs`): `ats::board(url)` returns `Board { vendor, token, host }` from board URLs *and* embed URLs (`boards.greenhouse.io/embed/job_board/js?for=<token>`). Its `key()` is `vendor/token`, so `boards.` and `job-boards.greenhouse.io` count as one board. Attributed boards are stored as `domains.ats` + `domains.ats_token`, which is the input for this milestone.

Implement as a trait:
```rust
trait AtsProvider {
    fn detect(url: &Url) -> Option<AtsBoard>;           // pure, testable
    async fn fetch_jobs(&self, board: &AtsBoard) -> Result<Vec<Job>>;
}
```
Verify endpoints when implementing — these change occasionally.

**Implemented (milestone 6, `crawler/src/extract/`).** Endpoints verified against live boards on 2026-09-30:
- Greenhouse: `boards-api.greenhouse.io/v1/boards/<token>/jobs?content=true&pay_transparency=true`.
  - `content` is entity-escaped HTML.
  - `company_name` names the board.
  - `pay_input_ranges` gives salary in cents with no interval; we assume annual unless the range title says hourly. 2,238/2,397 of Anduril's jobs had one.
- Lever: `api.lever.co/v0/postings/<co>?mode=json`. Plain array; `workplaceType`, ISO `country`, `salaryRange`. No company name. Palantir's listing is over 5 MB, hence the separate `max_resource_bytes` (50 MiB) for API and sitemap fetches.
- Ashby: `api.ashbyhq.com/posting-api/job-board/<org>?includeCompensation=true`. `compensation.summaryComponents` (Salary + interval); `isListed: false` postings are skipped. No company name.
- Missing boards return 404 on all three, recorded as `boards.last_status = 'not_found'`.
- robots.txt: Greenhouse disallows only `/embed/`, Lever allows all, and Ashby's robots.txt returns 401, which RFC 9309 treats as "allow".

**How boards are harvested:**
- Any frontier URL on a board with an API (landing page, posting, application form) triggers **one API fetch for the whole board** instead of HTML crawling.
- Posting links collapse into the board URL when enqueued.
- A board is re-fetched after `board_refresh_hours`. Jobs missing from a fresh listing get `closed_at`; reappearing ones reopen.
- Boards live in a `boards` table (key `vendor/token`). **Attribution** to a company domain happens when a company page embeds or links the board (milestone 5), or in reverse at harvest time: the ATS company name exactly equals a known domain's name, or the token exactly equals a domain's first label. Only `company`/`probing` domains count, and there's no fuzzy matching. Attaching a board moves its existing jobs to the domain.

A board whose API returns 404 is **detached** from its company (`jobs::detach_board`): the domain's `ats`/`ats_token`, and a careers URL pointing at that board, are cleared, so the real board can be attributed later. Seen with `greenhouse/paystack`, a name match for a board that doesn't exist.

Observed: 150 pages from the default seeds → ~1,500–3,300 jobs depending on which boards the crawl reaches (Anduril 2,397, OpenAI 838, Harvey 295, ElevenLabs 181, …). About 65% of Ashby jobs and most Greenhouse jobs carry a salary. Most board jobs have no company domain yet: they were found via portfolio pages, which is exactly the attribution gap an LLM could close later.

## 2. schema.org `JobPosting` JSON-LD
Parse every `<script type="application/ld+json">`, handle arrays and `@graph`. **Implemented** (`extract/json_ld.rs`): a posting without its own `url` takes the page URL only if it's the page's sole posting. A page that *is* one posting gets `pages.kind = 'job'`. Found on Paystack, Okta, Stripe, Pinterest and Fenris careers pages. Fields: `title, datePosted, validThrough, employmentType, hiringOrganization, jobLocation, jobLocationType (TELECOMMUTE), baseSalary, description, url`. Google requires this for job search, so many sites have it.

## 3. HTML heuristics (fallback)
On a careers page: find repeated sibling structures (lists/cards) whose links look like `/jobs/<slug>`, `/careers/<id>`, `/positions/...`. Title = anchor text; location/department from nearby text. Low confidence — mark `source = 'heuristic'`.

## 4. LLM extraction (fallback of the fallback)
When the heuristics find job-like links but the structure is messy, send the cleaned page text to the LLM and get back `Vec<Job>` (see `10-llm.md`). Mark it `source = 'llm'`.

## Normalized Job model
`id, company_domain, title, location, remote (bool/unknown), department, employment_type, salary_min/max/currency, posted_at, url (canonical apply URL), description (text/html), source (ats:<name> | jsonld | heuristic | llm), first_seen, last_seen, content_hash`.

Enrichment fields for NL search (category, seniority, geo, normalized salary, skills) are covered in `10-llm.md`.

Dedup on `(company_domain, canonical url)`; update `last_seen` on re-crawl; mark closed when missing from a later full board fetch.
