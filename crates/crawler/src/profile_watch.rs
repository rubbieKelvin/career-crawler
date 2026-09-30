//! Keeps the crawler in step with the active CV profile. The UI (or `crawler profile`)
//! writes the profile; this task notices, loads it into the shared handle the scorer and the
//! enricher read, recomputes job matches if they're stale, re-scores the queued frontier, and
//! reports a `profile_changed` event.

use std::sync::Arc;
use std::time::Duration;

use career_core::events::{self, Event};
use career_core::{frontier, matching, profile};
use sqlx::SqlitePool;
use url::Url;

use crate::control::CrawlControl;
use crate::steer::{SharedProfile, Steering};

const POLL: Duration = Duration::from_secs(2);
/// Queued URLs re-scored per transaction.
const RESCORE_BATCH: i64 = 500;

pub async fn run(pool: SqlitePool, shared: SharedProfile, control: Arc<CrawlControl>) {
    loop {
        if let Err(e) = sync(&pool, &shared).await {
            tracing::error!(error = %e, "profile sync failed");
        }
        tokio::select! {
            () = control.stopped() => return,
            () = tokio::time::sleep(POLL) => {}
        }
    }
}

/// One look at the database. Returns the event it recorded, if the profile changed.
pub async fn sync(pool: &SqlitePool, shared: &SharedProfile) -> anyhow::Result<Option<Event>> {
    let stored = profile::active(pool).await?;
    let Some(stored) = stored else {
        if shared.version().is_some() {
            shared.clear();
            let rescored = rescore_frontier(pool, &Steering::default()).await?;
            tracing::info!(rescored, "profile removed; frontier scores reset");
        }
        return Ok(None);
    };
    let version = (stored.id, stored.updated_at);
    if shared.version() == Some(version) && !stored.matches_stale() {
        return Ok(None);
    }

    let merged = stored.merged();
    shared.set(stored.id, stored.updated_at, merged.clone());
    let jobs_scored = if stored.matches_stale() {
        let n = matching::rescore_all(pool, stored.id, &merged).await?;
        profile::mark_matched(pool, stored.id, stored.updated_at).await?;
        n
    } else {
        0
    };
    let frontier_rescored = rescore_frontier(pool, &shared.steering()).await?;
    let event = Event::ProfileChanged {
        profile_id: stored.id,
        name: stored.name,
        source: stored.source,
        jobs_scored,
        frontier_rescored,
    };
    events::append(pool, &event).await?;
    tracing::info!(jobs_scored, frontier_rescored, "profile changed");
    return Ok(Some(event));
}

/// Swaps the profile part of every waiting URL's score. Returns how many changed.
pub async fn rescore_frontier(pool: &SqlitePool, steering: &Steering) -> anyhow::Result<usize> {
    let mut after = 0;
    let mut changed = 0;
    loop {
        let batch = frontier::waiting(pool, after, RESCORE_BATCH).await?;
        let Some(last) = batch.last() else {
            return Ok(changed);
        };
        after = last.rowid;
        let mut tx = pool.begin().await?;
        for row in &batch {
            let Ok(url) = Url::parse(&row.url) else {
                continue;
            };
            let new = steering
                .boost(&url, row.anchor.as_deref().unwrap_or(""))
                .points;
            if (new - row.profile_boost).abs() > f64::EPSILON {
                frontier::replace_profile_boost(&mut *tx, &row.url, row.profile_boost, new).await?;
                changed += 1;
            }
        }
        tx.commit().await?;
    }
}

#[cfg(test)]
mod tests {
    use career_core::db;
    use career_core::profile::{Overrides, Profile, ProfilePlace, WeightedSkill};

    use super::*;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_with(&dir.path().join("t.db"), 1).await.unwrap();
        return (dir, pool);
    }

    fn me() -> Profile {
        return Profile {
            titles: vec!["Backend Engineer".into()],
            industries: vec!["fintech".into()],
            skills: vec![WeightedSkill {
                name: "rust".into(),
                weight: 1.0,
            }],
            locations: vec![ProfilePlace {
                name: "Lagos, NG".into(),
                country_code: Some("NG".into()),
                lat: Some(6.52),
                lon: Some(3.38),
            }],
            ..Profile::default()
        };
    }

    async fn queue(pool: &SqlitePool, url: &str, score: f64, anchor: &str) {
        let parsed = Url::parse(url).unwrap();
        sqlx::query("INSERT INTO frontier (url, score, depth, state, enqueued_at, host) VALUES (?, ?, 1, 'queued', 1, ?)")
            .bind(url)
            .bind(score)
            .bind(parsed.host_str())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO domains (host, first_seen) VALUES ('src.example', 1) ON CONFLICT DO NOTHING")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pages (url, domain_id) VALUES (?, (SELECT id FROM domains LIMIT 1)) ON CONFLICT DO NOTHING")
            .bind(format!("https://src.example/{anchor}"))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO page_links (src_page_id, dst_url, anchor_text) VALUES ((SELECT id FROM pages WHERE url = ?), ?, ?)")
            .bind(format!("https://src.example/{anchor}"))
            .bind(url)
            .bind(anchor)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn score(pool: &SqlitePool, url: &str) -> f64 {
        return sqlx::query_scalar("SELECT score FROM frontier WHERE url = ?")
            .bind(url)
            .fetch_one(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_new_profile_loads_scores_jobs_and_rescores_the_frontier() {
        let (_dir, pool) = pool().await;
        let shared = SharedProfile::default();
        assert_eq!(
            sync(&pool, &shared).await.unwrap(),
            None,
            "no profile, nothing to do"
        );

        queue(
            &pool,
            "https://a.example/lists/lagos-fintech",
            10.0,
            "Lagos fintech startups",
        )
        .await;
        queue(&pool, "https://b.example/recipes", 10.0, "Recipes").await;
        sqlx::query("INSERT INTO jobs (url, title, category, skills, source, first_seen, last_seen) VALUES ('https://j/1', 'Backend Engineer', 'engineering', '[\"rust\"]', 'jsonld', 1, 1)")
            .execute(&pool)
            .await
            .unwrap();
        let id = profile::insert(&pool, "cv", "parser", "h", "t", &me())
            .await
            .unwrap();

        let event = sync(&pool, &shared)
            .await
            .unwrap()
            .expect("the profile is new");
        assert!(matches!(
            event,
            Event::ProfileChanged { profile_id, jobs_scored: 1, frontier_rescored: 1, .. } if profile_id == id
        ));
        assert!(score(&pool, "https://a.example/lists/lagos-fintech").await > 10.0);
        assert_eq!(score(&pool, "https://b.example/recipes").await, 10.0);
        let matches: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM job_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(matches, 1);
        assert!(
            !profile::active(&pool)
                .await
                .unwrap()
                .unwrap()
                .matches_stale()
        );
        assert!(shared.active().is_some());

        // Nothing changed: a quiet second look.
        assert_eq!(sync(&pool, &shared).await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_edit_swaps_the_boost_without_stacking_it() {
        let (_dir, pool) = pool().await;
        let shared = SharedProfile::default();
        queue(
            &pool,
            "https://a.example/lists/lagos-fintech",
            10.0,
            "Lagos fintech startups",
        )
        .await;
        let id = profile::insert(&pool, "cv", "parser", "h", "t", &me())
            .await
            .unwrap();
        sync(&pool, &shared).await.unwrap();
        let boosted = score(&pool, "https://a.example/lists/lagos-fintech").await;

        // Same profile re-synced (e.g. after a restart): the score doesn't grow.
        shared.clear();
        sync(&pool, &shared).await.unwrap();
        assert_eq!(
            score(&pool, "https://a.example/lists/lagos-fintech").await,
            boosted
        );

        // The user drops the fintech interest and the place: the boost goes away.
        profile::set_overrides(
            &pool,
            id,
            &Overrides {
                industries: Some(vec![]),
                locations: Some(vec![]),
                ..Overrides::default()
            },
        )
        .await
        .unwrap();
        let event = sync(&pool, &shared).await.unwrap().unwrap();
        assert!(matches!(
            event,
            Event::ProfileChanged {
                frontier_rescored: 1,
                ..
            }
        ));
        assert_eq!(
            score(&pool, "https://a.example/lists/lagos-fintech").await,
            10.0
        );
    }

    #[tokio::test]
    async fn removing_the_profile_resets_scores() {
        let (_dir, pool) = pool().await;
        let shared = SharedProfile::default();
        queue(
            &pool,
            "https://a.example/lists/lagos-fintech",
            10.0,
            "Lagos fintech startups",
        )
        .await;
        profile::insert(&pool, "cv", "parser", "h", "t", &me())
            .await
            .unwrap();
        sync(&pool, &shared).await.unwrap();
        assert!(score(&pool, "https://a.example/lists/lagos-fintech").await > 10.0);
        sqlx::query("UPDATE profiles SET active = 0")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(sync(&pool, &shared).await.unwrap(), None);
        assert_eq!(
            score(&pool, "https://a.example/lists/lagos-fintech").await,
            10.0
        );
        assert!(shared.active().is_none());
    }
}
