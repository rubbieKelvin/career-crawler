//! Applicant-tracking-system (ATS) job boards. Many companies share one ATS domain
//! (`jobs.ashbyhq.com/<company>`, `<company>.bamboohr.com`), so budgets and scoring work
//! per **board** rather than per registrable domain. Milestone 6 adds API fetching per board
//! (see `brainstorms/03-job-extraction.md`).

use url::Url;

/// Hosts where the company is the first path segment: `jobs.lever.co/<company>`.
const PATH_TENANT_HOSTS: &[&str] = &[
    "boards.greenhouse.io",
    "job-boards.greenhouse.io",
    "job-boards.eu.greenhouse.io",
    "jobs.lever.co",
    "jobs.eu.lever.co",
    "jobs.ashbyhq.com",
    "jobs.smartrecruiters.com",
    "apply.workable.com",
    "jobs.jobvite.com",
    "ats.rippling.com",
];

/// Domains where the company is the subdomain: `<company>.recruitee.com`.
const SUBDOMAIN_TENANT_SUFFIXES: &[&str] = &[
    "myworkdayjobs.com",
    "bamboohr.com",
    "recruitee.com",
    "teamtailor.com",
    "breezy.hr",
    "icims.com",
    "jobs.personio.de",
    "jobs.personio.com",
    "applytojob.com",
];

/// Subdomains of ATS vendors that are their own product, not a customer's board.
const VENDOR_SUBDOMAINS: &[&str] = &[
    "www", "app", "api", "help", "support", "status", "blog", "docs", "go",
];

/// A stable key for the company board a URL belongs to, e.g. `jobs.ashbyhq.com/atlys` or
/// `acme.bamboohr.com`. `None` if the URL isn't on a known ATS board.
pub fn board_key(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    if PATH_TENANT_HOSTS.contains(&host) {
        let tenant = url.path_segments()?.find(|s| !s.is_empty())?;
        return Some(format!("{host}/{}", tenant.to_ascii_lowercase()));
    }
    for suffix in SUBDOMAIN_TENANT_SUFFIXES {
        let Some(sub) = host.strip_suffix(suffix).and_then(|h| h.strip_suffix('.')) else {
            continue;
        };
        let first_label = sub.split('.').next().unwrap_or(sub);
        if !sub.is_empty() && !VENDOR_SUBDOMAINS.contains(&first_label) {
            return Some(host.to_string());
        }
    }
    return None;
}

pub fn is_board(url: &Url) -> bool {
    return board_key(url).is_some();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> Option<String> {
        return board_key(&Url::parse(s).unwrap());
    }

    #[test]
    fn path_tenants() {
        assert_eq!(
            key("https://jobs.ashbyhq.com/Atlys/6d94/application").as_deref(),
            Some("jobs.ashbyhq.com/atlys")
        );
        assert_eq!(
            key("https://boards.greenhouse.io/mixpanel").as_deref(),
            Some("boards.greenhouse.io/mixpanel")
        );
        assert_eq!(
            key("https://jobs.lever.co/acme/123").as_deref(),
            Some("jobs.lever.co/acme")
        );
        assert_eq!(
            key("https://jobs.lever.co/"),
            None,
            "the bare host is not a board"
        );
    }

    #[test]
    fn subdomain_tenants() {
        assert_eq!(
            key("https://acme.bamboohr.com/careers").as_deref(),
            Some("acme.bamboohr.com")
        );
        assert_eq!(
            key("https://acme.wd5.myworkdayjobs.com/en-US/External").as_deref(),
            Some("acme.wd5.myworkdayjobs.com")
        );
        assert_eq!(
            key("https://acme.jobs.personio.de/").as_deref(),
            Some("acme.jobs.personio.de")
        );
    }

    #[test]
    fn vendor_sites_are_not_boards() {
        for url in [
            "https://www.teamtailor.com/en/career-site/",
            "https://app.teamtailor.com/companies/x/dashboard",
            "https://teamtailor.com/",
            "https://www.greenhouse.io/careers",
            "https://acme.com/careers",
        ] {
            assert_eq!(key(url), None, "{url}");
        }
    }
}
