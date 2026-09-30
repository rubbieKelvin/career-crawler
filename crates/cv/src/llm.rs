//! The LLM path from CV text to a `Profile`. The model's JSON is validated field by field
//! (allowed seniorities, weights in range, places through the offline city table), and
//! whatever it leaves out is filled from the parser's reading.

use career_core::enrich;
use career_core::profile::{Profile, WeightedSkill};
use career_llm::tasks::{CV_PROFILE, CvProfile};
use career_llm::{Llm, LlmError};

use crate::parse::place_from_text;

/// CV characters sent to the model.
const MAX_CV_CHARS: usize = 12_000;
const MAX_LIST: usize = 25;

pub async fn extract(llm: &Llm, text: &str) -> Result<CvProfile, LlmError> {
    let input: String = text.chars().take(MAX_CV_CHARS).collect();
    return Ok(llm
        .complete_json::<CvProfile>(&CV_PROFILE, &input)
        .await?
        .value);
}

fn clean_list(items: &[String], max_len: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let item: String = item.trim().chars().take(max_len).collect();
        if !item.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(&item)) {
            out.push(item);
        }
        if out.len() >= MAX_LIST {
            break;
        }
    }
    return out;
}

/// The model's answer as a `Profile`, with gaps filled from `fallback` (the parser's
/// reading of the same CV).
pub fn into_profile(answer: &CvProfile, fallback: &Profile) -> Profile {
    let mut skills: Vec<WeightedSkill> = Vec::new();
    for s in &answer.skills {
        let name = s.name.trim().to_lowercase();
        if name.is_empty() || name.len() > 40 || skills.iter().any(|k| k.name == name) {
            continue;
        }
        skills.push(WeightedSkill {
            name,
            weight: s
                .weight
                .filter(|w| w.is_finite())
                .unwrap_or(0.5)
                .clamp(0.1, 1.0),
        });
        if skills.len() >= MAX_LIST {
            break;
        }
    }
    let locations: Vec<_> = clean_list(&answer.locations, 80)
        .iter()
        .map(|l| place_from_text(l))
        .collect();
    let titles = clean_list(&answer.titles, 80);
    let industries: Vec<String> = clean_list(&answer.industries, 40)
        .into_iter()
        .map(|i| i.to_lowercase())
        .collect();
    return Profile {
        seniority: enrich::one_of(answer.seniority.as_deref(), enrich::SENIORITIES)
            .map(str::to_string)
            .or_else(|| fallback.seniority.clone()),
        years_experience: answer
            .years_experience
            .filter(|y| y.is_finite() && (0.0..=60.0).contains(y))
            .or(fallback.years_experience),
        remote: enrich::one_of(answer.remote.as_deref(), enrich::REMOTE_MODES)
            .map(str::to_string)
            .or_else(|| fallback.remote.clone()),
        relocate: answer.relocate,
        salary_expectation_usd: answer
            .salary_expectation_usd
            .filter(|s| s.is_finite() && *s > 0.0),
        languages: clean_list(&answer.languages, 30),
        titles: if titles.is_empty() {
            fallback.titles.clone()
        } else {
            titles
        },
        skills: if skills.is_empty() {
            fallback.skills.clone()
        } else {
            skills
        },
        industries: if industries.is_empty() {
            fallback.industries.clone()
        } else {
            industries
        },
        locations: if locations.is_empty() {
            fallback.locations.clone()
        } else {
            locations
        },
        ..Profile::default()
    };
}

#[cfg(test)]
mod tests {
    use career_llm::tasks::CvSkill;

    use super::*;

    #[test]
    fn validates_the_answer_and_fills_gaps_from_the_parser() {
        let fallback = Profile {
            titles: vec!["Parser Title".into()],
            seniority: Some("mid".into()),
            years_experience: Some(3.0),
            ..Profile::default()
        };
        let answer = CvProfile {
            titles: vec![" Backend Engineer ".into(), "backend engineer".into()],
            seniority: Some("Wizard".into()),
            years_experience: Some(-4.0),
            skills: vec![
                CvSkill {
                    name: "Rust".into(),
                    weight: Some(7.0),
                },
                CvSkill {
                    name: "rust".into(),
                    weight: Some(0.2),
                },
                CvSkill {
                    name: " ".into(),
                    weight: None,
                },
                CvSkill {
                    name: "SQL".into(),
                    weight: None,
                },
            ],
            locations: vec!["Lagos, Nigeria".into()],
            remote: Some("sometimes".into()),
            salary_expectation_usd: Some(f64::NAN),
            ..CvProfile::default()
        };
        let p = into_profile(&answer, &fallback);
        assert_eq!(p.titles, ["Backend Engineer"]);
        assert_eq!(
            p.seniority.as_deref(),
            Some("mid"),
            "an invalid level falls back"
        );
        assert_eq!(
            p.years_experience,
            Some(3.0),
            "an impossible number falls back"
        );
        assert_eq!(p.skills.len(), 2);
        assert_eq!(
            (p.skills[0].name.as_str(), p.skills[0].weight),
            ("rust", 1.0)
        );
        assert_eq!(p.skills[1].weight, 0.5, "a missing weight is middling");
        assert_eq!(p.locations[0].name, "Lagos, NG");
        assert_eq!(p.remote, None);
        assert_eq!(p.salary_expectation_usd, None);
    }

    #[test]
    fn an_empty_answer_is_the_parsers_profile() {
        let fallback = Profile {
            titles: vec!["Designer".into()],
            skills: vec![WeightedSkill {
                name: "figma".into(),
                weight: 0.8,
            }],
            ..Profile::default()
        };
        let p = into_profile(&CvProfile::default(), &fallback);
        assert_eq!(p.titles, ["Designer"]);
        assert_eq!(p.skills, fallback.skills);
    }
}
