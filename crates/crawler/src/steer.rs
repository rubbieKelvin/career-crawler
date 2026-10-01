//! Profile-aware crawl steering (see `brainstorms/12-cv-profile.md`, section 3). Pure
//! scoring on top of the heuristic link score: a bonus for links that mention the
//! profile's industries and places, a preference for careers links to the departments its
//! titles belong to, and a "this neighbourhood yields" bonus for sources that already
//! produced well-matching jobs. It only ever adds to a score; nothing is excluded.

use std::sync::{Arc, RwLock};

use areer_core::enrich::{self, geo};
use areer_core::profile::Profile;
use url::Url;

const TOPIC_POINTS: f64 = 3.0;
const MAX_TOPIC_POINTS: f64 = 9.0;
const DEPARTMENT_POINTS: f64 = 6.0;
const YIELD_POINTS_PER_HIT: f64 = 2.0;
const MAX_YIELD_POINTS: f64 = 10.0;

/// Path segments that mean "this is the careers section".
const CAREERS_SEGMENTS: &[&str] = &[
    "careers",
    "career",
    "jobs",
    "job",
    "positions",
    "openings",
    "vacancies",
    "join-us",
    "work-with-us",
    "opportunities",
];

/// Words that name each job category in a department link (`/careers/engineering`).
const DEPARTMENT_WORDS: &[(&str, &[&str])] = &[
    (
        "engineering",
        &[
            "engineering",
            "engineers",
            "developers",
            "software",
            "technology",
            "tech",
        ],
    ),
    ("data", &["data", "analytics", "machine learning"]),
    ("product", &["product"]),
    ("design", &["design"]),
    ("sales", &["sales"]),
    ("marketing", &["marketing", "growth"]),
    ("customer_support", &["support", "customer success"]),
    ("operations", &["operations"]),
    ("finance", &["finance"]),
    ("hr", &["people", "recruiting", "talent"]),
    ("legal", &["legal"]),
    ("security", &["security"]),
    ("it", &["it"]),
];

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Boost {
    pub points: f64,
    pub reasons: Vec<&'static str>,
}

/// What the profile makes worth crawling.
#[derive(Debug, Default)]
pub struct Steering {
    /// Industries, cities and countries, lowercase.
    topics: Vec<String>,
    /// Department words of the categories the profile's titles fall in.
    departments: Vec<&'static str>,
}

fn words(s: &str) -> String {
    return s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
}

fn mentions(haystack: &str, term: &str) -> bool {
    return format!(" {haystack} ").contains(&format!(" {term} "));
}

impl Steering {
    pub fn from_profile(profile: &Profile) -> Self {
        let mut topics: Vec<String> = Vec::new();
        let mut add = |t: String| {
            if t.len() >= 3 && !topics.contains(&t) {
                topics.push(t);
            }
        };
        for industry in &profile.industries {
            add(words(industry));
        }
        for place in &profile.locations {
            if let Some(city) = place.name.split(',').next() {
                add(words(city));
            }
            if let Some(country) = place.country_code.as_deref().and_then(geo::country_name) {
                add(words(country));
            }
        }
        let categories: Vec<&str> = profile
            .titles
            .iter()
            .filter_map(|t| enrich::category(t, None))
            .collect();
        let departments = DEPARTMENT_WORDS
            .iter()
            .filter(|(category, _)| categories.contains(category))
            .flat_map(|(_, words)| words.iter().copied())
            .collect();
        return Self {
            topics,
            departments,
        };
    }

    /// The bonus for a link, from its URL and anchor text.
    pub fn boost(&self, url: &Url, anchor: &str) -> Boost {
        let mut boost = Boost::default();
        if self.topics.is_empty() && self.departments.is_empty() {
            return boost;
        }
        let path = words(url.path());
        let haystack = format!("{} {path}", words(anchor));

        let topic_hits = self
            .topics
            .iter()
            .filter(|t| mentions(&haystack, t))
            .count();
        if topic_hits > 0 {
            boost.points += (topic_hits as f64 * TOPIC_POINTS).min(MAX_TOPIC_POINTS);
            boost.reasons.push("profile_topic");
        }

        let in_careers = url.path_segments().is_some_and(|mut segs| {
            segs.any(|s| CAREERS_SEGMENTS.contains(&s.to_lowercase().as_str()))
        });
        if in_careers && self.departments.iter().any(|d| mentions(&haystack, d)) {
            boost.points += DEPARTMENT_POINTS;
            boost.reasons.push("profile_department");
        }
        return boost;
    }
}

/// Extra score for links from a source that already yielded `hits` well-matching jobs.
pub fn yield_points(hits: i64) -> f64 {
    return (hits.max(0) as f64 * YIELD_POINTS_PER_HIT).min(MAX_YIELD_POINTS);
}

#[derive(Debug, Default)]
struct State {
    /// `(profile id, updated_at)` last loaded.
    version: Option<(i64, i64)>,
    profile: Option<Arc<Profile>>,
    steering: Arc<Steering>,
}

/// The active profile, shared between the tasks that use it and the watcher that refreshes
/// it. Cheap to clone; with no profile it changes nothing.
#[derive(Debug, Clone, Default)]
pub struct SharedProfile(Arc<RwLock<State>>);

impl SharedProfile {
    pub fn set(&self, id: i64, updated_at: i64, profile: Profile) {
        let steering = Arc::new(Steering::from_profile(&profile));
        let mut state = self.0.write().unwrap();
        *state = State {
            version: Some((id, updated_at)),
            profile: Some(Arc::new(profile)),
            steering,
        };
    }

    pub fn clear(&self) {
        *self.0.write().unwrap() = State::default();
    }

    pub fn version(&self) -> Option<(i64, i64)> {
        return self.0.read().unwrap().version;
    }

    /// The active profile's id and merged profile.
    pub fn active(&self) -> Option<(i64, Arc<Profile>)> {
        let state = self.0.read().unwrap();
        return state
            .version
            .zip(state.profile.clone())
            .map(|((id, _), p)| (id, p));
    }

    pub fn steering(&self) -> Arc<Steering> {
        return self.0.read().unwrap().steering.clone();
    }
}

#[cfg(test)]
mod tests {
    use areer_core::profile::ProfilePlace;

    use super::*;

    fn profile() -> Profile {
        return Profile {
            titles: vec!["Backend Engineer".into()],
            industries: vec!["fintech".into()],
            locations: vec![ProfilePlace {
                name: "Lagos, NG".into(),
                country_code: Some("NG".into()),
                lat: None,
                lon: None,
            }],
            ..Profile::default()
        };
    }

    fn boost(steering: &Steering, url: &str, anchor: &str) -> Boost {
        return steering.boost(&Url::parse(url).unwrap(), anchor);
    }

    #[test]
    fn topic_links_get_a_bonus_capped_at_three_terms() {
        let s = Steering::from_profile(&profile());
        let b = boost(
            &s,
            "https://x.com/lists/nigerian-fintech-startups",
            "Lagos fintech companies",
        );
        assert!(b.points >= 3.0 && b.reasons == ["profile_topic"], "{b:?}");
        assert_eq!(
            boost(&s, "https://x.com/recipes", "Best jollof").points,
            0.0
        );
        // "Nigeria" is not "nigerian", but Lagos and fintech both hit here.
        let all = boost(&s, "https://x.com/nigeria/lagos/fintech", "");
        assert_eq!(all.points, MAX_TOPIC_POINTS);
    }

    #[test]
    fn terms_match_whole_words_only() {
        let s = Steering::from_profile(&profile());
        assert_eq!(
            boost(&s, "https://x.com/", "fintechnology news").points,
            0.0
        );
    }

    #[test]
    fn matching_department_links_under_careers_are_preferred() {
        let s = Steering::from_profile(&profile());
        let eng = boost(&s, "https://acme.com/careers/engineering", "Engineering");
        assert_eq!(eng.reasons, ["profile_department"]);
        assert_eq!(
            boost(&s, "https://acme.com/careers/sales", "Sales").points,
            0.0
        );
        assert_eq!(
            boost(&s, "https://acme.com/engineering-blog", "Engineering blog").points,
            0.0,
            "outside the careers section a department word means nothing"
        );
    }

    #[test]
    fn no_profile_no_change() {
        let s = Steering::default();
        assert_eq!(
            boost(&s, "https://acme.com/careers/engineering", "Engineering").points,
            0.0
        );
        assert_eq!(
            Steering::from_profile(&Profile::default())
                .boost(&Url::parse("https://a.com/jobs/engineering").unwrap(), "")
                .points,
            0.0
        );
    }

    #[test]
    fn yield_grows_then_caps() {
        assert_eq!(yield_points(0), 0.0);
        assert_eq!(yield_points(2), 4.0);
        assert_eq!(yield_points(50), MAX_YIELD_POINTS);
        assert_eq!(yield_points(-3), 0.0);
    }

    #[test]
    fn shared_profile_swaps_atomically() {
        let shared = SharedProfile::default();
        assert!(shared.active().is_none());
        shared.set(3, 100, profile());
        assert_eq!(shared.version(), Some((3, 100)));
        assert_eq!(shared.active().unwrap().0, 3);
        assert!(
            shared
                .steering()
                .boost(&Url::parse("https://a.com/fintech").unwrap(), "")
                .points
                > 0.0
        );
        shared.clear();
        assert!(shared.active().is_none());
        assert_eq!(
            shared
                .steering()
                .boost(&Url::parse("https://a.com/fintech").unwrap(), "")
                .points,
            0.0
        );
    }
}
