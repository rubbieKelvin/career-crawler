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

Implement as a trait:
```rust
trait AtsProvider {
    fn detect(url: &Url) -> Option<AtsBoard>;           // pure, testable
    async fn fetch_jobs(&self, board: &AtsBoard) -> Result<Vec<Job>>;
}
```
Verify endpoints when implementing — these change occasionally.

## 2. schema.org `JobPosting` JSON-LD
Parse every `<script type="application/ld+json">`, handle arrays and `@graph`. Fields: `title, datePosted, validThrough, employmentType, hiringOrganization, jobLocation, jobLocationType (TELECOMMUTE), baseSalary, description, url`. Google requires this for job search, so many sites have it.

## 3. HTML heuristics (fallback)
On a careers page: find repeated sibling structures (lists/cards) whose links look like `/jobs/<slug>`, `/careers/<id>`, `/positions/...`. Title = anchor text; location/department from nearby text. Low confidence — mark `source = 'heuristic'`.

## 4. LLM extraction (fallback of the fallback)
When the heuristics find job-like links but the structure is messy, send the cleaned page text to the LLM and get back `Vec<Job>` (see `10-llm.md`). Mark it `source = 'llm'`.

## Normalized Job model
`id, company_domain, title, location, remote (bool/unknown), department, employment_type, salary_min/max/currency, posted_at, url (canonical apply URL), description (text/html), source (ats:<name> | jsonld | heuristic | llm), first_seen, last_seen, content_hash`.

Enrichment fields for NL search (category, seniority, geo, normalized salary, skills) are covered in `10-llm.md`.

Dedup on `(company_domain, canonical url)`; update `last_seen` on re-crawl; mark closed when missing from a later full board fetch.
