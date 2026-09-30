//! Domain classification status, shared by the crawler (which sets it) and the UI (which
//! colours graph nodes by it). Stored in `domains.status`.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DomainStatus {
    /// Seen as a link target but never fetched.
    #[default]
    Discovered,
    /// Fetched, but not enough evidence yet either way.
    Probing,
    /// Looks like a company site: gets the harvest budget.
    Company,
    /// Homepage seen and it doesn't look like a company (or is parked).
    NotCompany,
}

impl DomainStatus {
    pub fn as_str(self) -> &'static str {
        return match self {
            DomainStatus::Discovered => "discovered",
            DomainStatus::Probing => "probing",
            DomainStatus::Company => "company",
            DomainStatus::NotCompany => "not_company",
        };
    }

    /// Unknown strings (e.g. statuses added by later milestones) read as `Discovered`.
    pub fn parse(s: &str) -> Self {
        return match s {
            "probing" => DomainStatus::Probing,
            "company" => DomainStatus::Company,
            "not_company" => DomainStatus::NotCompany,
            _ => DomainStatus::Discovered,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for s in [
            DomainStatus::Discovered,
            DomainStatus::Probing,
            DomainStatus::Company,
            DomainStatus::NotCompany,
        ] {
            assert_eq!(DomainStatus::parse(s.as_str()), s);
        }
        assert_eq!(DomainStatus::parse("whatever"), DomainStatus::Discovered);
    }
}
