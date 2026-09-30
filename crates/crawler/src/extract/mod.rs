//! Job extraction (see `brainstorms/03-job-extraction.md`), most reliable source first:
//! 1. ATS board APIs (Greenhouse, Lever, Ashby): one request lists a company's open jobs
//! 2. schema.org `JobPosting` JSON-LD embedded in any HTML page
//!
//! HTML heuristics and the LLM tier come in later milestones. Everything here is pure
//! parsing; fetching and storage live in `crawl` and `store`.

pub mod ashby;
pub mod greenhouse;
pub mod json_ld;
pub mod lever;

use career_core::jobs::Job;
use chrono::{DateTime, NaiveDate, NaiveDateTime};
use url::Url;

use crate::ats::{Board, Vendor};
use crate::parse;

/// Longest description we store, in characters.
const MAX_DESCRIPTION_CHARS: usize = 20_000;

/// A board's open jobs as its API reports them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BoardJobs {
    /// Company name, if the API says.
    pub company: Option<String>,
    pub jobs: Vec<Job>,
}

/// Whether we can read this vendor's boards through an API instead of crawling HTML.
pub fn has_api(vendor: Vendor) -> bool {
    return matches!(vendor, Vendor::Greenhouse | Vendor::Lever | Vendor::Ashby);
}

/// The API endpoint listing all of a board's open jobs.
pub fn api_url(board: &Board) -> Option<Url> {
    let url = match board.vendor {
        Vendor::Greenhouse => greenhouse::api_url(&board.token),
        Vendor::Lever => lever::api_url(&board.token),
        Vendor::Ashby => ashby::api_url(&board.token),
        _ => return None,
    };
    return Url::parse(&url).ok();
}

pub fn parse_board(board: &Board, body: &[u8]) -> anyhow::Result<BoardJobs> {
    return match board.vendor {
        Vendor::Greenhouse => greenhouse::parse(body),
        Vendor::Lever => lever::parse(body),
        Vendor::Ashby => ashby::parse(body),
        other => anyhow::bail!("no API parser for {}", other.as_str()),
    };
}

/// HTML (possibly entity-escaped, as Greenhouse sends it) to plain text, capped.
pub fn html_to_text(html: &str) -> String {
    let mut text = parse::fragment_text(html);
    // `&lt;p&gt;hi&lt;/p&gt;` decodes to markup on the first pass.
    if text.contains("</") || text.contains("<p") || text.contains("<br") {
        text = parse::fragment_text(&text);
    }
    return truncate(&text, MAX_DESCRIPTION_CHARS);
}

pub fn truncate(text: &str, max_chars: usize) -> String {
    return text.chars().take(max_chars).collect();
}

/// Trimmed, or `None` if empty.
pub fn clean(s: Option<&str>) -> Option<String> {
    return s
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
}

/// RFC 3339 timestamps, naive datetimes (as UTC) and plain dates, to epoch milliseconds.
pub fn parse_datetime_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(dt.and_utc().timestamp_millis());
    }
    let date = NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()?;
    return Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
}

/// `FullTime`, `FULL_TIME`, `Full-time`, `full time` → `full_time`, and so on.
pub fn employment_type(s: &str) -> Option<String> {
    let key: String = s
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase();
    let normalized = match key.as_str() {
        "fulltime" | "permanent" => "full_time",
        "parttime" => "part_time",
        "contract" | "contractor" | "freelance" => "contract",
        "intern" | "internship" => "internship",
        "temporary" | "temp" | "fixedterm" => "temporary",
        "volunteer" => "volunteer",
        "" => return None,
        _ => return Some(s.trim().to_ascii_lowercase().replace([' ', '-'], "_")),
    };
    return Some(normalized.to_string());
}

/// `Remote`, `hybrid`, `OnSite`, `on-site`, `in office` → `remote` / `hybrid` / `onsite`.
pub fn remote_mode(s: &str) -> Option<String> {
    let key: String = s
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase();
    let mode = match key.as_str() {
        "remote" | "telecommute" | "fullyremote" => "remote",
        "hybrid" => "hybrid",
        "onsite" | "inoffice" | "office" | "inperson" => "onsite",
        _ => return None,
    };
    return Some(mode.to_string());
}

/// Salary intervals from any source (`1 YEAR`, `per-year-salary`, `YEAR`, `HOUR`) → `year`, `hour`, ….
pub fn salary_period(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    for (needle, period) in [
        ("year", "year"),
        ("annual", "year"),
        ("month", "month"),
        ("week", "week"),
        ("day", "day"),
        ("hour", "hour"),
    ] {
        if lower.contains(needle) {
            return Some(period.to_string());
        }
    }
    return None;
}

/// ISO alpha-2 country codes only. Country names are resolved in milestone 10 enrichment.
pub fn country_code(s: &str) -> Option<String> {
    let s = s.trim();
    if s.len() == 2 && s.chars().all(|c| c.is_ascii_alphabetic()) {
        return Some(s.to_ascii_uppercase());
    }
    return None;
}

/// Canonical form of a posting URL, matching the frontier's normalization.
pub fn posting_url(raw: &str) -> Option<String> {
    let url = Url::parse(raw.trim()).ok()?;
    return Some(career_core::urls::normalize(&url).to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_handles_escaped_markup() {
        assert_eq!(
            html_to_text("<p>Build <b>things</b></p><ul><li>Rust</li></ul>"),
            "Build things Rust"
        );
        assert_eq!(
            html_to_text("&lt;p&gt;We&amp;#39;re &lt;strong&gt;hiring&lt;/strong&gt;&lt;/p&gt;"),
            "We're hiring"
        );
    }

    #[test]
    fn dates() {
        assert_eq!(
            parse_datetime_ms("2026-09-21T19:46:54-04:00"),
            Some(1790034414000)
        );
        assert_eq!(
            parse_datetime_ms("2026-04-15T13:26:54.184+00:00"),
            Some(1776259614184)
        );
        assert_eq!(parse_datetime_ms("2024-01-05"), Some(1704412800000));
        assert_eq!(
            parse_datetime_ms("2024-01-05T10:00:00"),
            Some(1704448800000)
        );
        assert_eq!(parse_datetime_ms("soon"), None);
    }

    #[test]
    fn normalizers() {
        for (raw, want) in [
            ("FullTime", "full_time"),
            ("FULL_TIME", "full_time"),
            ("Full-time", "full_time"),
            ("Intern", "internship"),
        ] {
            assert_eq!(employment_type(raw).as_deref(), Some(want), "{raw}");
        }
        assert_eq!(
            employment_type("Seasonal work").as_deref(),
            Some("seasonal_work")
        );
        assert_eq!(remote_mode("OnSite").as_deref(), Some("onsite"));
        assert_eq!(remote_mode("on-site").as_deref(), Some("onsite"));
        assert_eq!(remote_mode("TELECOMMUTE").as_deref(), Some("remote"));
        assert_eq!(remote_mode("flexible"), None);
        assert_eq!(salary_period("1 YEAR").as_deref(), Some("year"));
        assert_eq!(salary_period("per-hour-wage").as_deref(), Some("hour"));
        assert_eq!(country_code("sg").as_deref(), Some("SG"));
        assert_eq!(country_code("Canada"), None);
        assert_eq!(
            posting_url("https://a16z.com/about/jobs/?utm_source=x&gh_jid=7").as_deref(),
            Some("https://a16z.com/about/jobs/?gh_jid=7")
        );
    }
}
