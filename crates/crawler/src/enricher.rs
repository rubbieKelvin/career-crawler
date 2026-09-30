//! Job enrichment (see `brainstorms/10-llm.md`): every stored job gets the normalized
//! fields search needs. The rules in `career_core::enrich` always run; the LLM is asked only
//! for jobs the rules couldn't fully place or categorize, in batches, and its answer is
//! validated field by field before it's merged (rules win where they have a value).
//!
//! Runs as a background task next to the crawl, and as `crawler enrich` to backfill a
//! database (for instance after turning the LLM on).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use career_core::enrich::{self, Enrichment, JobFacts, geo};
use career_core::events::{self, Event};
use career_core::matching;
use career_llm::tasks::{ENRICH_JOBS, EnrichedJob, EnrichedJobs};
use career_llm::{Llm, LlmError};
use serde_json::json;
use sqlx::SqlitePool;

use crate::control::CrawlControl;
use crate::steer::SharedProfile;

/// Jobs looked at per pass.
const PASS_LIMIT: i64 = 100;
/// Description characters shown to the model per job.
const DESCRIPTION_CHARS: usize = 600;
const MAX_SKILLS: usize = 12;
const IDLE: Duration = Duration::from_secs(2);
/// How long to wait when the daily token budget is spent.
const BUDGET_WAIT: Duration = Duration::from_secs(60);

pub struct Enricher {
    pub pool: SqlitePool,
    pub llm: Option<Arc<Llm>>,
    /// Jobs per LLM call.
    pub batch_size: usize,
    /// The active CV profile: freshly enriched jobs are matched against it.
    pub profile: SharedProfile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pass {
    /// Jobs written this pass.
    pub written: usize,
    /// Of those, jobs the LLM contributed to.
    pub by_llm: usize,
    /// The token budget ran out during this pass.
    pub budget_exhausted: bool,
}

/// One job on its way through a pass.
struct Work {
    facts: JobFacts,
    found: Enrichment,
    /// Already went through the rules in an earlier pass and is waiting for the LLM.
    retry: bool,
    wants_llm: bool,
    /// `Some(true)` = answered, `Some(false)` = tried and failed, `None` = not attempted.
    llm: Option<bool>,
}

impl Enricher {
    /// Works until told to stop, sleeping when there is nothing to do.
    pub async fn run(self, control: Arc<CrawlControl>) {
        loop {
            let wait = match self.pass(true).await {
                Ok(pass) if pass.budget_exhausted => {
                    tracing::warn!(
                        "LLM token budget used up; job enrichment continues with rules only"
                    );
                    Some(BUDGET_WAIT)
                }
                Ok(pass) if pass.written > 0 => None,
                Ok(_) => Some(IDLE),
                Err(e) => {
                    tracing::error!(error = %e, "job enrichment pass failed");
                    Some(IDLE)
                }
            };
            if control.stop_requested() {
                return;
            }
            if let Some(wait) = wait {
                tokio::select! {
                    () = control.stopped() => return,
                    () = tokio::time::sleep(wait) => {}
                }
            }
        }
    }

    /// Passes until nothing more can be done. `use_llm = false` runs the rules only.
    pub async fn drain(&self, use_llm: bool) -> anyhow::Result<Pass> {
        let mut total = Pass::default();
        loop {
            let pass = self.pass(use_llm).await?;
            total.written += pass.written;
            total.by_llm += pass.by_llm;
            total.budget_exhausted |= pass.budget_exhausted;
            if pass.written == 0 || pass.budget_exhausted {
                return Ok(total);
            }
        }
    }

    pub async fn pass(&self, use_llm: bool) -> anyhow::Result<Pass> {
        let llm = self.llm.as_ref().filter(|_| use_llm);
        let rows = enrich::pending(&self.pool, llm.is_some(), PASS_LIMIT).await?;
        if rows.is_empty() {
            return Ok(Pass::default());
        }
        let mut work: Vec<Work> = rows
            .into_iter()
            .map(|(facts, state)| {
                let found = enrich::rules(&facts);
                let wants_llm = enrich::needs_llm(&facts, &found);
                return Work {
                    facts,
                    found,
                    retry: state == "rules",
                    wants_llm,
                    llm: None,
                };
            })
            .collect();

        let mut budget_exhausted = false;
        if let Some(llm) = llm {
            let wanting: Vec<usize> = (0..work.len()).filter(|&i| work[i].wants_llm).collect();
            for chunk in wanting.chunks(self.batch_size.max(1)) {
                if budget_exhausted {
                    break;
                }
                budget_exhausted = ask_batch(llm, &mut work, chunk).await;
            }
        }

        let mut tx = self.pool.begin().await?;
        let mut written = 0;
        let mut by_llm = 0;
        let mut written_ids = Vec::new();
        for w in &work {
            // A job waiting for the LLM that never got its turn is left as it is.
            if w.retry && w.llm.is_none() {
                continue;
            }
            let done = !w.wants_llm || w.llm == Some(true);
            let failed = w.llm == Some(false);
            let state = if done { "done" } else { "rules" };
            enrich::write(&mut tx, w.facts.id, &w.found, state, failed).await?;
            written += 1;
            written_ids.push(w.facts.id);
            by_llm += usize::from(w.llm == Some(true));
        }
        if let Some((profile_id, profile)) = self.profile.active() {
            matching::score_jobs(&mut tx, profile_id, &profile, &written_ids).await?;
        }
        if written > 0 {
            let event = Event::JobsEnriched {
                total: written,
                llm: by_llm,
            };
            events::append(&mut *tx, &event).await?;
        }
        tx.commit().await?;
        if written > 0 {
            tracing::info!(written, by_llm, "jobs enriched");
        }
        return Ok(Pass {
            written,
            by_llm,
            budget_exhausted,
        });
    }
}

/// Asks the LLM about one batch and merges the answers. Returns whether the token budget
/// ran out (the rest of the pass then skips the LLM).
async fn ask_batch(llm: &Llm, work: &mut [Work], chunk: &[usize]) -> bool {
    let postings: Vec<serde_json::Value> = chunk
        .iter()
        .map(|&i| {
            let f = &work[i].facts;
            let description = f
                .description
                .as_deref()
                .map(|d| d.chars().take(DESCRIPTION_CHARS).collect::<String>());
            return json!({
                "id": f.id,
                "title": f.title,
                "department": f.department,
                "location": f.location,
                "description": description,
            });
        })
        .collect();
    let input = json!(postings).to_string();

    let answers: HashMap<i64, EnrichedJob> = match llm
        .complete_json::<EnrichedJobs>(&ENRICH_JOBS, &input)
        .await
    {
        Ok(done) => done.value.jobs.into_iter().map(|j| (j.id, j)).collect(),
        Err(LlmError::BudgetExhausted) => return true,
        Err(e) => {
            tracing::warn!(error = %e, jobs = chunk.len(), "LLM enrichment failed; keeping rule-based fields");
            HashMap::new()
        }
    };
    for &i in chunk {
        let w = &mut work[i];
        match answers.get(&w.facts.id) {
            Some(answer) => {
                merge(&mut w.found, answer);
                w.llm = Some(true);
            }
            None => w.llm = Some(false),
        }
    }
    return false;
}

/// Folds a validated LLM answer into the rule-based result. A value the rules already have
/// is kept: they are deterministic and were derived from the same text.
fn merge(found: &mut Enrichment, answer: &EnrichedJob) {
    if found.category.is_none() {
        found.category =
            enrich::one_of(answer.category.as_deref(), enrich::CATEGORIES).map(str::to_string);
    }
    if found.seniority.is_none() {
        found.seniority =
            enrich::one_of(answer.seniority.as_deref(), enrich::SENIORITIES).map(str::to_string);
    }
    for skill in &answer.skills {
        let skill = skill.trim().to_lowercase();
        if !skill.is_empty()
            && skill.len() <= 40
            && found.skills.len() < MAX_SKILLS
            && !found.skills.contains(&skill)
        {
            found.skills.push(skill);
        }
    }
    if found.remote_mode.is_none() {
        found.remote_mode =
            enrich::one_of(answer.remote_mode.as_deref(), enrich::REMOTE_MODES).map(str::to_string);
    }
    if found.country_code.is_none() && found.city.is_none() {
        let country = enrich::country_code(answer.country_code.as_deref());
        let city = answer
            .city
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty());
        // Coordinates only from our own gazetteer: the model is not a source of them.
        let known = city.and_then(|c| geo::city(c, country.as_deref()));
        found.country_code = country.or_else(|| known.map(|k| k.country.clone()));
        found.city = city.map(|c| match known {
            Some(k) => k.name.clone(),
            None => c.chars().take(80).collect(),
        });
        found.region = known.and_then(|k| k.region.clone()).or_else(|| {
            answer
                .region
                .as_deref()
                .map(|r| r.trim().chars().take(80).collect::<String>())
                .filter(|r| !r.is_empty())
        });
        found.lat = known.map(|k| k.lat);
        found.lon = known.map(|k| k.lon);
    }
    if matches!(found.remote_mode.as_deref(), Some("remote" | "hybrid")) {
        for region in answer
            .remote_regions
            .iter()
            .filter_map(|r| enrich::remote_region(r))
        {
            if !found.remote_regions.contains(&region) {
                found.remote_regions.push(region);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use career_core::config::LlmConfig;
    use career_core::db;
    use career_core::jobs::{self, Job};
    use career_llm::testing::FakeProvider;

    use super::*;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        return (dir, pool);
    }

    async fn add(pool: &SqlitePool, url: &str, title: &str, location: Option<&str>) -> i64 {
        let job = Job {
            url: url.into(),
            title: title.into(),
            location: location.map(str::to_string),
            source: "jsonld".into(),
            ..Job::default()
        };
        let mut conn = pool.acquire().await.unwrap();
        jobs::upsert(&mut conn, &job, None, None, 1).await.unwrap();
        return sqlx::query_scalar("SELECT id FROM jobs WHERE url = ?")
            .bind(url)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    }

    type Row = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<f64>,
        String,
        i64,
    );

    async fn row(pool: &SqlitePool, id: i64) -> Row {
        return sqlx::query_as(
            "SELECT category, seniority, country_code, lat, enrich_state, enrich_attempts FROM jobs WHERE id = ?",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    }

    fn enricher(pool: &SqlitePool, provider: Arc<FakeProvider>) -> Enricher {
        let llm = Llm::new(&LlmConfig::default(), pool.clone(), provider);
        return Enricher {
            pool: pool.clone(),
            llm: Some(Arc::new(llm)),
            batch_size: 10,
            profile: SharedProfile::default(),
        };
    }

    fn rules_only(pool: &SqlitePool) -> Enricher {
        return Enricher {
            pool: pool.clone(),
            llm: None,
            batch_size: 10,
            profile: SharedProfile::default(),
        };
    }

    #[tokio::test]
    async fn rules_alone_enrich_and_finish_clear_jobs() {
        let (_dir, pool) = pool().await;
        let id = add(
            &pool,
            "https://j/1",
            "Senior Backend Engineer",
            Some("Lagos, Nigeria"),
        )
        .await;
        let e = rules_only(&pool);
        let pass = e.drain(true).await.unwrap();
        assert_eq!((pass.written, pass.by_llm), (1, 0));
        let (category, seniority, country, lat, state, _) = row(&pool, id).await;
        assert_eq!(category.as_deref(), Some("engineering"));
        assert_eq!(seniority.as_deref(), Some("senior"));
        assert_eq!(country.as_deref(), Some("NG"));
        assert!(lat.is_some());
        assert_eq!(state, "done");
        let events: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE kind = 'jobs_enriched'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(events, 1);
        assert_eq!(e.pass(true).await.unwrap().written, 0, "nothing left to do");
    }

    #[tokio::test]
    async fn the_llm_is_asked_only_about_jobs_the_rules_could_not_finish() {
        let (_dir, pool) = pool().await;
        let clear = add(
            &pool,
            "https://j/1",
            "Senior Backend Engineer",
            Some("Lagos, Nigeria"),
        )
        .await;
        let odd = add(
            &pool,
            "https://j/2",
            "Beekeeper",
            Some("Ikoyi Peninsula / Remote (EMEA)"),
        )
        .await;
        let provider = Arc::new(FakeProvider::new(move |req| {
            let input = &req.messages[1].content;
            assert!(
                input.contains("Beekeeper") && !input.contains("Backend"),
                "{input}"
            );
            return Ok(format!(
                r#"{{"jobs": [{{"id": {odd}, "category": "operations", "seniority": null, "skills": ["Bees", "  "],
                    "city": "Lagos", "region": null, "country_code": "ng", "remote_mode": "hybrid",
                    "remote_regions": ["emea", "NG", "global", "the moon"]}}]}}"#
            ));
        }));
        let e = enricher(&pool, provider.clone());
        let pass = e.drain(true).await.unwrap();
        assert_eq!((pass.written, pass.by_llm), (2, 1));
        assert_eq!(provider.call_count(), 1);

        assert_eq!(row(&pool, clear).await.4, "done");
        let (category, _, country, lat, state, attempts) = row(&pool, odd).await;
        assert_eq!(category.as_deref(), Some("operations"));
        assert_eq!(country.as_deref(), Some("NG"));
        assert!(
            (lat.unwrap() - 6.52).abs() < 0.01,
            "coordinates come from the gazetteer"
        );
        assert_eq!((state.as_str(), attempts), ("done", 0));
        let (skills, mode, regions): (String, String, String) =
            sqlx::query_as("SELECT skills, remote_mode, remote_regions FROM jobs WHERE id = ?")
                .bind(odd)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(skills, r#"["bees"]"#);
        assert_eq!(
            mode, "remote",
            "the location text already said so; rules win over the model"
        );
        assert_eq!(regions, r#"["EMEA","NG","global"]"#);
    }

    #[tokio::test]
    async fn junk_in_the_answer_is_dropped_field_by_field() {
        let (_dir, pool) = pool().await;
        let id = add(&pool, "https://j/1", "Beekeeper", Some("Middle of nowhere")).await;
        let provider = Arc::new(FakeProvider::new(move |_| {
            return Ok(format!(
                r#"{{"jobs": [{{"id": {id}, "category": "astronaut", "seniority": "god-tier",
                    "city": "Atlantis", "country_code": "Nigeria", "remote_mode": "sometimes"}}]}}"#
            ));
        }));
        enricher(&pool, provider).drain(true).await.unwrap();
        let (category, seniority, country, lat, state, _) = row(&pool, id).await;
        assert_eq!(
            (category, seniority, country, lat),
            (None, None, None, None)
        );
        let city: Option<String> = sqlx::query_scalar("SELECT city FROM jobs WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            city.as_deref(),
            Some("Atlantis"),
            "the name is kept, without invented coordinates"
        );
        assert_eq!(
            state, "done",
            "the model did answer; it just had nothing usable"
        );
    }

    #[tokio::test]
    async fn a_failing_llm_falls_back_to_rules_and_retries_a_bounded_number_of_times() {
        let (_dir, pool) = pool().await;
        let id = add(
            &pool,
            "https://j/1",
            "Beekeeper",
            Some("Ikoyi Peninsula, Sector 7"),
        )
        .await;
        let provider = Arc::new(FakeProvider::replying("not json at all"));
        let e = enricher(&pool, provider.clone());

        let first = e.pass(true).await.unwrap();
        assert_eq!((first.written, first.by_llm), (1, 0));
        let (_, _, _, _, state, attempts) = row(&pool, id).await;
        assert_eq!((state.as_str(), attempts), ("rules", 1));

        // Each further pass retries, until the attempts are used up.
        e.pass(true).await.unwrap();
        e.pass(true).await.unwrap();
        assert_eq!(row(&pool, id).await.5, enrich::MAX_LLM_ATTEMPTS);
        let calls = provider.call_count();
        assert_eq!(e.pass(true).await.unwrap().written, 0);
        assert_eq!(provider.call_count(), calls, "gave up");
    }

    #[tokio::test]
    async fn an_exhausted_token_budget_leaves_jobs_waiting_without_spinning() {
        let (_dir, pool) = pool().await;
        let id = add(
            &pool,
            "https://j/1",
            "Beekeeper",
            Some("Ikoyi Peninsula, Sector 7"),
        )
        .await;
        let provider = Arc::new(FakeProvider::replying(r#"{"jobs": []}"#));
        let config = LlmConfig {
            daily_token_budget: 1,
            ..LlmConfig::default()
        };
        // Spend the budget with an unrelated call.
        sqlx::query(
            "INSERT INTO llm_calls (ts, task, prompt_version, model, cache_key, status, tokens_in)
             VALUES (?, 'x', 1, 'm', 'k', 'ok', 5)",
        )
        .bind(career_core::time::now_ms())
        .execute(&pool)
        .await
        .unwrap();
        let e = Enricher {
            pool: pool.clone(),
            llm: Some(Arc::new(Llm::new(&config, pool.clone(), provider.clone()))),
            batch_size: 10,
            profile: SharedProfile::default(),
        };
        let first = e.pass(true).await.unwrap();
        assert!(first.budget_exhausted);
        assert_eq!(
            row(&pool, id).await.4,
            "rules",
            "rule-based fields are still written"
        );
        // Next pass: only the waiting job remains, and it's left alone.
        let second = e.pass(true).await.unwrap();
        assert_eq!(second.written, 0);
        assert_eq!(provider.call_count(), 0);
        assert_eq!(
            row(&pool, id).await.5,
            0,
            "running out of budget isn't the job's fault"
        );
    }

    #[tokio::test]
    async fn a_changed_posting_is_enriched_again_and_an_unchanged_one_is_not() {
        let (_dir, pool) = pool().await;
        let id = add(&pool, "https://j/1", "Engineer", Some("Lagos, Nigeria")).await;
        let e = rules_only(&pool);
        e.drain(false).await.unwrap();
        assert_eq!(row(&pool, id).await.4, "done");

        add(&pool, "https://j/1", "Engineer", Some("Lagos, Nigeria")).await;
        assert_eq!(
            row(&pool, id).await.4,
            "done",
            "a re-crawl of the same posting keeps its enrichment"
        );
        assert_eq!(row(&pool, id).await.2.as_deref(), Some("NG"));

        add(
            &pool,
            "https://j/1",
            "Senior Engineer",
            Some("Accra, Ghana"),
        )
        .await;
        assert_eq!(row(&pool, id).await.4, "pending");
        e.drain(false).await.unwrap();
        let (_, seniority, country, _, _, _) = row(&pool, id).await;
        assert_eq!(
            (seniority.as_deref(), country.as_deref()),
            (Some("senior"), Some("GH"))
        );
    }

    #[tokio::test]
    async fn newly_enriched_jobs_are_matched_against_the_active_profile() {
        use career_core::profile::{self, Profile, WeightedSkill};
        let (_dir, pool) = pool().await;
        let id = add(
            &pool,
            "https://j/1",
            "Senior Backend Engineer",
            Some("Lagos, Nigeria"),
        )
        .await;
        let mine = Profile {
            titles: vec!["Backend Engineer".into()],
            seniority: Some("senior".into()),
            skills: vec![WeightedSkill {
                name: "rust".into(),
                weight: 1.0,
            }],
            ..Profile::default()
        };
        let pid = profile::insert(&pool, "p", "parser", "h", "t", &mine)
            .await
            .unwrap();
        let shared = SharedProfile::default();
        shared.set(pid, 1, mine);
        let e = Enricher {
            profile: shared,
            ..rules_only(&pool)
        };
        e.drain(false).await.unwrap();
        let score: f64 = sqlx::query_scalar("SELECT score FROM job_matches WHERE job_id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(score > 0.5, "{score}");

        // Without a profile nothing is written.
        let other = add(&pool, "https://j/2", "Accountant", None).await;
        rules_only(&pool).drain(false).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM job_matches WHERE job_id = ?")
            .bind(other)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }
}
