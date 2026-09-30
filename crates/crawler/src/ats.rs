//! Applicant-tracking-system (ATS) job boards. Many companies share one ATS domain
//! (`jobs.ashbyhq.com/<company>`, `<company>.bamboohr.com`), so budgets and scoring work
//! per **board**: a vendor plus the company's token on it. Milestone 6 adds API fetching
//! per board (see `brainstorms/03-job-extraction.md`).

use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Greenhouse,
    Lever,
    Ashby,
    SmartRecruiters,
    Workable,
    Jobvite,
    Rippling,
    Workday,
    BambooHr,
    Recruitee,
    Teamtailor,
    Breezy,
    Icims,
    Personio,
    JazzHr,
}

impl Vendor {
    pub fn as_str(self) -> &'static str {
        return match self {
            Vendor::Greenhouse => "greenhouse",
            Vendor::Lever => "lever",
            Vendor::Ashby => "ashby",
            Vendor::SmartRecruiters => "smartrecruiters",
            Vendor::Workable => "workable",
            Vendor::Jobvite => "jobvite",
            Vendor::Rippling => "rippling",
            Vendor::Workday => "workday",
            Vendor::BambooHr => "bamboohr",
            Vendor::Recruitee => "recruitee",
            Vendor::Teamtailor => "teamtailor",
            Vendor::Breezy => "breezy",
            Vendor::Icims => "icims",
            Vendor::Personio => "personio",
            Vendor::JazzHr => "jazzhr",
        };
    }
}

/// Hosts where the company token is the first path segment: `jobs.lever.co/<company>`.
const PATH_TENANT_HOSTS: &[(&str, Vendor)] = &[
    ("boards.greenhouse.io", Vendor::Greenhouse),
    ("job-boards.greenhouse.io", Vendor::Greenhouse),
    ("job-boards.eu.greenhouse.io", Vendor::Greenhouse),
    ("jobs.lever.co", Vendor::Lever),
    ("jobs.eu.lever.co", Vendor::Lever),
    ("jobs.ashbyhq.com", Vendor::Ashby),
    ("jobs.smartrecruiters.com", Vendor::SmartRecruiters),
    ("apply.workable.com", Vendor::Workable),
    ("jobs.jobvite.com", Vendor::Jobvite),
    ("ats.rippling.com", Vendor::Rippling),
];

/// Domains where the company token is the subdomain: `<company>.recruitee.com`.
const SUBDOMAIN_TENANT_SUFFIXES: &[(&str, Vendor)] = &[
    ("myworkdayjobs.com", Vendor::Workday),
    ("bamboohr.com", Vendor::BambooHr),
    ("recruitee.com", Vendor::Recruitee),
    ("teamtailor.com", Vendor::Teamtailor),
    ("breezy.hr", Vendor::Breezy),
    ("icims.com", Vendor::Icims),
    ("jobs.personio.de", Vendor::Personio),
    ("jobs.personio.com", Vendor::Personio),
    ("applytojob.com", Vendor::JazzHr),
];

/// Subdomains of ATS vendors that are their own product, not a customer's board.
const VENDOR_SUBDOMAINS: &[&str] = &[
    "www", "app", "api", "help", "support", "status", "blog", "docs", "go",
];

/// Path segments that are ATS machinery rather than a company token.
const NON_TENANT_SEGMENTS: &[&str] = &["embed", "api", "v1", "static", "assets"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board {
    pub vendor: Vendor,
    /// The company's identifier on the vendor, lowercased: `acme` for `jobs.lever.co/acme`.
    pub token: String,
    /// For subdomain tenants, the full host (Workday needs `acme.wd5.myworkdayjobs.com`).
    pub host: String,
}

impl Board {
    /// Stable identity across the vendor's hosts and URL shapes, e.g. `greenhouse/acme`.
    pub fn key(&self) -> String {
        return format!("{}/{}", self.vendor.as_str(), self.token);
    }

    /// The board's public landing page.
    pub fn url(&self) -> Url {
        let url = match self.vendor {
            Vendor::Greenhouse => format!("https://job-boards.greenhouse.io/{}", self.token),
            Vendor::Lever => format!("https://jobs.lever.co/{}", self.token),
            Vendor::Ashby => format!("https://jobs.ashbyhq.com/{}", self.token),
            _ if self.host.starts_with(&format!("{}.", self.token)) => {
                format!("https://{}/", self.host)
            }
            _ => format!("https://{}/{}", self.host, self.token),
        };
        return Url::parse(&url).expect("board URLs are built from valid parts");
    }

    /// Whether the token plausibly names the company at `domain`, e.g. `andurilindustries`
    /// for `anduril.com`. Used to decide whether a link from a site to a board is that
    /// site's own board, rather than one it merely links to (portfolio pages, news).
    pub fn matches_domain(&self, domain: &str) -> bool {
        let label: String = domain
            .split('.')
            .next()
            .unwrap_or_default()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase();
        let token: String = self
            .token
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect();
        if label.len() < 3 || token.len() < 3 {
            return label == token;
        }
        return token.contains(&label) || label.contains(&token);
    }
}

/// The ATS board a URL belongs to, if any, including embed URLs such as
/// `boards.greenhouse.io/embed/job_board/js?for=acme`.
pub fn board(url: &Url) -> Option<Board> {
    let host = url.host_str()?;
    if let Some((_, vendor)) = PATH_TENANT_HOSTS.iter().find(|(h, _)| *h == host) {
        let first = url.path_segments()?.find(|s| !s.is_empty())?;
        let token = if NON_TENANT_SEGMENTS.contains(&first) {
            // Greenhouse embeds carry the token in `?for=`.
            url.query_pairs()
                .find(|(k, _)| k == "for")
                .map(|(_, v)| v.into_owned())?
        } else {
            first.to_string()
        };
        return Some(Board {
            vendor: *vendor,
            token: token.to_ascii_lowercase(),
            host: host.to_string(),
        });
    }
    for (suffix, vendor) in SUBDOMAIN_TENANT_SUFFIXES {
        let Some(sub) = host.strip_suffix(suffix).and_then(|h| h.strip_suffix('.')) else {
            continue;
        };
        let first_label = sub.split('.').next().unwrap_or(sub);
        if !sub.is_empty() && !VENDOR_SUBDOMAINS.contains(&first_label) {
            return Some(Board {
                vendor: *vendor,
                token: first_label.to_ascii_lowercase(),
                host: host.to_string(),
            });
        }
    }
    return None;
}

pub fn is_board(url: &Url) -> bool {
    return board(url).is_some();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> Option<Board> {
        return board(&Url::parse(s).unwrap());
    }

    fn key(s: &str) -> Option<String> {
        return b(s).map(|b| b.key());
    }

    #[test]
    fn path_tenants() {
        assert_eq!(
            key("https://jobs.ashbyhq.com/Atlys/6d94/application").as_deref(),
            Some("ashby/atlys")
        );
        assert_eq!(
            key("https://boards.greenhouse.io/mixpanel").as_deref(),
            Some("greenhouse/mixpanel")
        );
        assert_eq!(
            key("https://job-boards.greenhouse.io/mixpanel/jobs/1").as_deref(),
            Some("greenhouse/mixpanel")
        );
        assert_eq!(
            key("https://jobs.lever.co/acme/123").as_deref(),
            Some("lever/acme")
        );
        assert_eq!(
            key("https://jobs.lever.co/"),
            None,
            "the bare host is not a board"
        );
    }

    #[test]
    fn greenhouse_embeds_use_the_for_param() {
        assert_eq!(
            key("https://boards.greenhouse.io/embed/job_board/js?for=acme").as_deref(),
            Some("greenhouse/acme")
        );
        assert_eq!(
            key("https://boards.greenhouse.io/embed/job_app?for=Acme&token=1").as_deref(),
            Some("greenhouse/acme")
        );
        assert_eq!(key("https://boards.greenhouse.io/embed/job_board"), None);
        assert_eq!(
            key("https://jobs.ashbyhq.com/acme/embed?version=2").as_deref(),
            Some("ashby/acme")
        );
    }

    #[test]
    fn subdomain_tenants() {
        assert_eq!(
            key("https://acme.bamboohr.com/careers").as_deref(),
            Some("bamboohr/acme")
        );
        let workday = b("https://acme.wd5.myworkdayjobs.com/en-US/External").unwrap();
        assert_eq!(
            (workday.key().as_str(), workday.host.as_str()),
            ("workday/acme", "acme.wd5.myworkdayjobs.com")
        );
        assert_eq!(
            key("https://acme.jobs.personio.de/").as_deref(),
            Some("personio/acme")
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

    #[test]
    fn canonical_urls() {
        assert_eq!(
            b("https://boards.greenhouse.io/embed/job_board?for=acme")
                .unwrap()
                .url()
                .as_str(),
            "https://job-boards.greenhouse.io/acme"
        );
        assert_eq!(
            b("https://acme.bamboohr.com/careers")
                .unwrap()
                .url()
                .as_str(),
            "https://acme.bamboohr.com/"
        );
        assert_eq!(
            b("https://apply.workable.com/acme/j/1")
                .unwrap()
                .url()
                .as_str(),
            "https://apply.workable.com/acme"
        );
    }

    #[test]
    fn token_matches_company_domain() {
        let board = |token: &str| Board {
            vendor: Vendor::Greenhouse,
            token: token.into(),
            host: String::new(),
        };
        assert!(board("andurilindustries").matches_domain("anduril.com"));
        assert!(board("stripe").matches_domain("stripe.com"));
        assert!(board("paystack").matches_domain("paystack.co"));
        assert!(!board("kong").matches_domain("a16z.com"));
        assert!(!board("ab").matches_domain("abc.com"));
    }
}
