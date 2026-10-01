//! The no-LLM path from CV text to a `Profile`: split it into sections by common headings,
//! then read skills from the shared skill taxonomy (`areer_core::enrich`), titles from
//! role-noun lines, years from date ranges, and places from the offline city table. Rough
//! by design; the LLM path is the accurate one, and the user can edit whatever this gets wrong.

use areer_core::enrich::{self, geo};
use areer_core::profile::{Profile, ProfilePlace, WeightedSkill};

const ROLE_WORDS: &[&str] = &[
    "engineer",
    "developer",
    "programmer",
    "designer",
    "manager",
    "analyst",
    "scientist",
    "architect",
    "consultant",
    "lead",
    "director",
    "specialist",
    "officer",
    "administrator",
    "accountant",
    "recruiter",
    "marketer",
    "coordinator",
    "researcher",
    "intern",
    "sre",
    "devops",
];
const MAX_TITLES: usize = 5;
const MAX_TITLE_WORDS: usize = 7;
const MAX_SKILLS: usize = 25;
/// Lines from the top of the CV searched for the candidate's location and headline.
const HEADER_LINES: usize = 12;

const INDUSTRIES: &[(&str, &[&str])] = &[
    ("fintech", &["fintech", "payments", "banking", "lending"]),
    ("e-commerce", &["e-commerce", "ecommerce", "marketplace"]),
    (
        "healthcare",
        &[
            "healthcare",
            "health tech",
            "healthtech",
            "medical",
            "hospital",
        ],
    ),
    ("education", &["edtech", "e-learning", "online learning"]),
    ("logistics", &["logistics", "supply chain", "delivery"]),
    ("saas", &["saas", "b2b software"]),
    ("gaming", &["gaming", "video games"]),
    ("telecom", &["telecom", "telecommunications"]),
    ("energy", &["energy", "solar", "oil and gas"]),
    ("media", &["media", "streaming", "publishing"]),
    ("agriculture", &["agritech", "agriculture", "farming"]),
    ("insurance", &["insurance", "insurtech"]),
    ("real estate", &["real estate", "proptech"]),
    ("consulting", &["consulting", "consultancy"]),
    ("government", &["government", "public sector"]),
    ("nonprofit", &["nonprofit", "non-profit", "ngo"]),
];

const LANGUAGES: &[&str] = &[
    "english",
    "french",
    "spanish",
    "german",
    "portuguese",
    "italian",
    "arabic",
    "hausa",
    "yoruba",
    "igbo",
    "swahili",
    "hindi",
    "mandarin",
    "chinese",
    "japanese",
    "korean",
    "russian",
    "dutch",
    "turkish",
    "polish",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Summary,
    Experience,
    Skills,
    Languages,
    Other,
}

/// A line that is just a section heading (`## Work Experience`, `SKILLS:`).
fn heading(line: &str) -> Option<Section> {
    let cleaned = line
        .trim()
        .trim_start_matches('#')
        .trim()
        .trim_end_matches(':')
        .to_lowercase();
    if cleaned.is_empty() || cleaned.len() > 32 {
        return None;
    }
    return Some(match cleaned.as_str() {
        "summary" | "profile" | "about" | "about me" | "professional summary" | "objective" => {
            Section::Summary
        }
        "experience"
        | "work experience"
        | "work history"
        | "employment"
        | "employment history"
        | "professional experience"
        | "career history" => Section::Experience,
        "skills" | "technical skills" | "core skills" | "key skills" | "skills & tools"
        | "technologies" | "tech stack" => Section::Skills,
        "languages" => Section::Languages,
        "education" | "projects" | "certifications" | "awards" | "interests" | "references"
        | "publications" | "volunteering" | "courses" => Section::Other,
        _ => return None,
    });
}

/// Lines grouped by section; text before the first heading is `Summary`.
fn sections(text: &str) -> Vec<(Section, Vec<&str>)> {
    let mut out: Vec<(Section, Vec<&str>)> = vec![(Section::Summary, Vec::new())];
    for line in text.lines() {
        if let Some(section) = heading(line) {
            out.push((section, Vec::new()));
        } else if let Some((_, lines)) = out.last_mut() {
            lines.push(line);
        }
    }
    return out;
}

fn section_text(sections: &[(Section, Vec<&str>)], wanted: Section) -> String {
    return sections
        .iter()
        .filter(|(s, _)| *s == wanted)
        .flat_map(|(_, lines)| lines.iter().copied())
        .collect::<Vec<_>>()
        .join("\n");
}

fn has_role_word(text: &str) -> bool {
    let lower = text.to_lowercase();
    return lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| ROLE_WORDS.contains(&w))
        || lower.contains("head of");
}

/// The role part of a line like "Senior Backend Engineer, Acme Ltd | 2020 – Present".
fn title_of(line: &str) -> Option<String> {
    let line = line.trim();
    if line.starts_with(['-', '•', '*', '·']) || line.ends_with('.') {
        return None;
    }
    let mut piece = line;
    for sep in [" at ", " @ ", " | ", " - ", " – ", " — ", ",", "(", "|"] {
        if let Some((head, _)) = piece.split_once(sep) {
            let rest = piece.split_once(sep).map(|(_, r)| r).unwrap_or("");
            // Keep whichever side holds the role ("Acme Ltd - Backend Engineer").
            piece = if has_role_word(head) || !has_role_word(rest) {
                head
            } else {
                rest
            };
        }
    }
    let words: Vec<&str> = piece
        .split_whitespace()
        .filter(|w| !w.chars().any(|c| c.is_ascii_digit()))
        .collect();
    if words.is_empty() || words.len() > MAX_TITLE_WORDS {
        return None;
    }
    let title = words.join(" ");
    return has_role_word(&title).then_some(title);
}

fn titles(sections: &[(Section, Vec<&str>)], text: &str) -> Vec<String> {
    let experience = section_text(sections, Section::Experience);
    let header: String = text
        .lines()
        .take(HEADER_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    let mut out: Vec<String> = Vec::new();
    // The headline under the name, then roles in the order the CV lists them.
    let source = if experience.is_empty() {
        text.to_string()
    } else {
        format!("{header}\n{experience}")
    };
    for line in source.lines() {
        if let Some(t) = title_of(line)
            && !out.iter().any(|o| o.eq_ignore_ascii_case(&t))
        {
            out.push(t);
        }
        if out.len() >= MAX_TITLES {
            break;
        }
    }
    return out;
}

fn skills(sections: &[(Section, Vec<&str>)], text: &str) -> Vec<WeightedSkill> {
    let skills_section = section_text(sections, Section::Skills);
    let experience = section_text(sections, Section::Experience);
    // The first third of the experience section is the most recent role(s).
    let recent: String = experience
        .chars()
        .take(experience.chars().count() / 3 + 1)
        .collect();
    let in_recent: Vec<&str> = enrich::skill_counts(&recent)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    let in_skills: Vec<&str> = enrich::skill_counts(&skills_section)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    return enrich::skill_counts(text)
        .into_iter()
        .take(MAX_SKILLS)
        .map(|(name, count)| {
            let mut weight = 0.3 + 0.12 * count.min(4) as f64;
            if in_recent.contains(&name) {
                weight += 0.15;
            }
            if in_skills.contains(&name) {
                weight += 0.05;
            }
            return WeightedSkill {
                name: name.to_string(),
                weight: (weight.min(1.0) * 100.0).round() / 100.0,
            };
        })
        .collect();
}

fn month(word: &str) -> bool {
    const MONTHS: &[&str] = &[
        "jan",
        "feb",
        "mar",
        "apr",
        "may",
        "jun",
        "jul",
        "aug",
        "sep",
        "sept",
        "oct",
        "nov",
        "dec",
        "january",
        "february",
        "march",
        "april",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    return MONTHS.contains(&word);
}

/// Years of experience: the union of the date ranges found ("Jan 2019 – Present",
/// "2015-2018"), so overlapping jobs count once.
pub fn years_of_experience(text: &str, current_year: i32) -> Option<f64> {
    let spaced = text.replace(['–', '—'], " - ").replace('-', " - ");
    let tokens: Vec<String> = spaced
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    let year = |t: &str| -> Option<i32> {
        return t
            .parse::<i32>()
            .ok()
            .filter(|y| (1970..=current_year).contains(y));
    };
    let mut ranges: Vec<(i32, i32)> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if let Some(start) = year(&tokens[i]) {
            let mut j = i + 1;
            if tokens.get(j).is_some_and(|t| t == "-" || t == "to") {
                j += 1;
                if tokens.get(j).is_some_and(|t| month(t)) {
                    j += 1;
                }
                let end = tokens.get(j).and_then(|t| match t.as_str() {
                    "present" | "current" | "now" | "date" => Some(current_year),
                    other => year(other),
                });
                if let Some(end) = end.filter(|e| *e >= start) {
                    ranges.push((start, end));
                    i = j;
                }
            }
        }
        i += 1;
    }
    if ranges.is_empty() {
        return None;
    }
    ranges.sort();
    let mut total = 0;
    let mut current = ranges[0];
    for &(s, e) in &ranges[1..] {
        if s <= current.1 {
            current.1 = current.1.max(e);
        } else {
            total += current.1 - current.0;
            current = (s, e);
        }
    }
    total += current.1 - current.0;
    return (total > 0).then_some(f64::from(total.min(50)));
}

fn industries(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    return INDUSTRIES
        .iter()
        .filter(|(_, words)| words.iter().any(|w| lower.contains(w)))
        .map(|(name, _)| name.to_string())
        .take(5)
        .collect();
}

/// Turns free text ("Lagos, Nigeria") into a place, keeping the text if it isn't known.
pub fn place_from_text(text: &str) -> ProfilePlace {
    let place = geo::geocode(text);
    let name = match (&place.city, &place.country_code) {
        (Some(city), Some(cc)) => format!("{city}, {cc}"),
        (None, Some(cc)) => cc.clone(),
        _ => text.trim().to_string(),
    };
    return ProfilePlace {
        name,
        country_code: place.country_code,
        lat: place.lat,
        lon: place.lon,
    };
}

fn location(text: &str) -> Option<ProfilePlace> {
    for line in text.lines().take(HEADER_LINES) {
        // Contact lines mix email, phone and address: try each piece on its own.
        for piece in line.split(['|', '•', '·']) {
            let place = geo::geocode(piece);
            if place.city.is_some() {
                return Some(place_from_text(piece));
            }
        }
    }
    return None;
}

fn languages(sections: &[(Section, Vec<&str>)]) -> Vec<String> {
    let text = section_text(sections, Section::Languages).to_lowercase();
    return LANGUAGES
        .iter()
        .filter(|l| text.split(|c: char| !c.is_alphabetic()).any(|w| w == **l))
        .map(|l| {
            let mut c = l.chars();
            return c.next().map_or(String::new(), |f| {
                f.to_uppercase().collect::<String>() + c.as_str()
            });
        })
        .collect();
}

pub fn profile_from_text(text: &str, current_year: i32) -> Profile {
    let sections = sections(text);
    let titles = titles(&sections, text);
    // Education dates are not experience.
    let experience = section_text(&sections, Section::Experience);
    let years = years_of_experience(
        if experience.is_empty() {
            text
        } else {
            &experience
        },
        current_year,
    );
    let head: String = text
        .lines()
        .take(HEADER_LINES)
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let summary = section_text(&sections, Section::Summary).to_lowercase();
    let remote = (head.contains("remote") || summary.contains("open to remote"))
        .then(|| "remote".to_string());
    return Profile {
        seniority: titles
            .first()
            .and_then(|t| enrich::seniority(t))
            .map(str::to_string),
        titles,
        years_experience: years,
        skills: skills(&sections, text),
        industries: industries(text),
        locations: location(text).into_iter().collect(),
        remote,
        languages: languages(&sections),
        ..Profile::default()
    };
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const CV: &str = "\
Jane Doe
Senior Backend Engineer
Lagos, Nigeria | jane@example.com | +234 000 000

## Summary
Backend engineer building payments APIs for fintech and e-commerce companies.

## Experience
Senior Backend Engineer, Paystack Ltd | Jan 2021 – Present
- Built payment services in Rust and PostgreSQL, deployed on AWS with Kubernetes.
- Led migration of Python services to Rust.

Software Engineer at Flutterwave
2017 - 2020
- Wrote Python and SQL data pipelines; Rust for the hot path.

Junior Developer - Andela (2015 - 2017)
- Ruby on Rails apps.

## Skills
Rust, Python, SQL, PostgreSQL, AWS, Kubernetes, Docker

## Education
BSc Computer Science, University of Lagos, 2011 - 2015

## Languages
English (fluent), Yoruba (native)
";

    #[test]
    fn reads_a_typical_cv() {
        let p = profile_from_text(CV, 2026);
        assert_eq!(
            p.titles,
            [
                "Senior Backend Engineer",
                "Software Engineer",
                "Junior Developer"
            ]
        );
        assert_eq!(p.seniority.as_deref(), Some("senior"));
        // 2015-2017 and 2017-2020 merge into 2015-2020 (5 years), plus 2021-2026 (5); the
        // 2011-2015 degree is not experience.
        assert_eq!(p.years_experience, Some(10.0));
        let names: Vec<&str> = p.skills.iter().map(|s| s.name.as_str()).collect();
        for want in [
            "rust",
            "python",
            "sql",
            "postgresql",
            "aws",
            "kubernetes",
            "docker",
        ] {
            assert!(names.contains(&want), "{want} in {names:?}");
        }
        assert_eq!(names[0], "rust", "the most-used skill comes first");
        let weight = |n: &str| p.skills.iter().find(|s| s.name == n).unwrap().weight;
        assert!(
            weight("rust") > weight("docker"),
            "recent, frequent skills weigh more"
        );
        assert!(p.skills.iter().all(|s| s.weight > 0.0 && s.weight <= 1.0));
        assert_eq!(p.industries, ["fintech", "e-commerce"]);
        assert_eq!(p.locations.len(), 1);
        assert_eq!(p.locations[0].name, "Lagos, NG");
        assert_eq!(p.locations[0].country_code.as_deref(), Some("NG"));
        assert!(p.locations[0].lat.is_some());
        assert_eq!(p.languages, ["English", "Yoruba"]);
    }

    #[test]
    fn years_come_from_merged_date_ranges() {
        assert_eq!(years_of_experience("Jan 2019 – Present", 2026), Some(7.0));
        assert_eq!(
            years_of_experience("2015-2018 and 2016-2019", 2026),
            Some(4.0)
        );
        assert_eq!(
            years_of_experience("2010 - 2012, 2020 - 2022", 2026),
            Some(4.0)
        );
        assert_eq!(
            years_of_experience("Born 1990. No jobs listed.", 2026),
            None
        );
        assert_eq!(
            years_of_experience("2018 to 2016", 2026),
            None,
            "backwards range"
        );
        assert_eq!(years_of_experience("Sept 2020 to date", 2026), Some(6.0));
        assert_eq!(
            years_of_experience("2020 to soon", 2026),
            None,
            "no usable end: ignored"
        );
    }

    #[test]
    fn a_cv_without_headings_still_yields_something() {
        let p = profile_from_text(
            "John Smith\nData Scientist\nBerlin\nI use Python, SQL and Spark daily.\n2018 - 2022 Data Scientist at Acme",
            2026,
        );
        assert_eq!(p.titles, ["Data Scientist"]);
        assert!(p.skills.iter().any(|s| s.name == "spark"));
        assert_eq!(p.locations[0].name, "Berlin, DE");
        assert_eq!(p.years_experience, Some(4.0));
    }

    #[test]
    fn gibberish_gives_an_empty_profile_not_a_crash() {
        let p = profile_from_text("lorem ipsum dolor sit amet", 2026);
        assert!(p.titles.is_empty() && p.skills.is_empty() && p.locations.is_empty());
        assert_eq!(p.years_experience, None);
    }

    #[test]
    fn bullets_and_sentences_are_not_titles() {
        assert_eq!(title_of("- Led a team of engineers"), None);
        assert_eq!(title_of("I was the engineer who fixed everything."), None);
        assert_eq!(
            title_of("Acme Ltd - Staff Engineer").as_deref(),
            Some("Staff Engineer")
        );
        assert_eq!(
            title_of("Product Manager (2019-2021)").as_deref(),
            Some("Product Manager")
        );
    }
}
