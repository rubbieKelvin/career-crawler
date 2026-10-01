//! The CV profile (see `brainstorms/12-cv-profile.md`): what the user is looking for, as
//! extracted from their CV and edited by hand. Stored in `profiles`; the crawler ranks jobs
//! and steers the crawl with it. Extraction lives in the `areer-cv` crate.

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::time::now_ms;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeightedSkill {
    pub name: String,
    /// In (0, 1]: how much it features in the CV (frequency and recency).
    pub weight: f64,
}

/// A place the user lives or would work in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfilePlace {
    /// As shown: "Lagos, NG".
    pub name: String,
    pub country_code: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// "Backend Engineer", most recent first.
    pub titles: Vec<String>,
    /// One of `enrich::SENIORITIES`.
    pub seniority: Option<String>,
    pub years_experience: Option<f64>,
    pub skills: Vec<WeightedSkill>,
    pub industries: Vec<String>,
    pub locations: Vec<ProfilePlace>,
    /// `onsite`, `hybrid` or `remote`; `None` means no preference.
    pub remote: Option<String>,
    pub relocate: bool,
    pub salary_expectation_usd: Option<f64>,
    pub languages: Vec<String>,
    /// Terms a job must mention (missing one caps its score). User-set only.
    pub must_have: Vec<String>,
    /// Terms that rule a job out. User-set only.
    pub exclude: Vec<String>,
    pub excluded_companies: Vec<String>,
}

/// The user's edits: a field that is `Some` replaces what the CV said, whatever a later
/// re-extraction finds.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Overrides {
    pub titles: Option<Vec<String>>,
    pub seniority: Option<String>,
    pub years_experience: Option<f64>,
    pub skills: Option<Vec<WeightedSkill>>,
    pub industries: Option<Vec<String>>,
    pub locations: Option<Vec<ProfilePlace>>,
    pub remote: Option<String>,
    pub relocate: Option<bool>,
    pub salary_expectation_usd: Option<f64>,
    pub languages: Option<Vec<String>>,
    pub must_have: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub excluded_companies: Option<Vec<String>>,
}

impl Profile {
    /// The extracted profile with the user's edits applied on top.
    pub fn merged(&self, o: &Overrides) -> Profile {
        let p = self.clone();
        return Profile {
            titles: o.titles.clone().unwrap_or(p.titles),
            seniority: o.seniority.clone().or(p.seniority),
            years_experience: o.years_experience.or(p.years_experience),
            skills: o.skills.clone().unwrap_or(p.skills),
            industries: o.industries.clone().unwrap_or(p.industries),
            locations: o.locations.clone().unwrap_or(p.locations),
            remote: o.remote.clone().or(p.remote),
            relocate: o.relocate.unwrap_or(p.relocate),
            salary_expectation_usd: o.salary_expectation_usd.or(p.salary_expectation_usd),
            languages: o.languages.clone().unwrap_or(p.languages),
            must_have: o.must_have.clone().unwrap_or(p.must_have),
            exclude: o.exclude.clone().unwrap_or(p.exclude),
            excluded_companies: o.excluded_companies.clone().unwrap_or(p.excluded_companies),
        };
    }

    /// Seniority as stated, else guessed from years of experience.
    pub fn effective_seniority(&self) -> Option<&str> {
        if let Some(s) = self.seniority.as_deref() {
            return Some(s);
        }
        return self.years_experience.map(|y| match y {
            y if y < 2.0 => "junior",
            y if y < 5.0 => "mid",
            y if y < 9.0 => "senior",
            _ => "lead",
        });
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredProfile {
    pub id: i64,
    pub name: String,
    pub source: String,
    pub cv_hash: Option<String>,
    pub extracted: Profile,
    pub overrides: Overrides,
    pub active: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub matched_at: Option<i64>,
}

impl StoredProfile {
    /// What the crawler and the matcher work from.
    pub fn merged(&self) -> Profile {
        return self.extracted.merged(&self.overrides);
    }

    /// The job ranking predates the last change.
    pub fn matches_stale(&self) -> bool {
        return self.matched_at.is_none_or(|m| m < self.updated_at);
    }
}

fn stored(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<StoredProfile> {
    return Ok(StoredProfile {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        source: row.try_get("source")?,
        cv_hash: row.try_get("cv_hash")?,
        extracted: serde_json::from_str(&row.try_get::<String, _>("extracted")?)?,
        overrides: serde_json::from_str(&row.try_get::<String, _>("overrides")?)?,
        active: row.try_get::<i64, _>("active")? != 0,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        matched_at: row.try_get("matched_at")?,
    });
}

const COLUMNS: &str =
    "id, name, source, cv_hash, extracted, overrides, active, created_at, updated_at, matched_at";

/// The active profile, if the user has given a CV.
pub async fn active(pool: &SqlitePool) -> anyhow::Result<Option<StoredProfile>> {
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM profiles WHERE active = 1 ORDER BY id DESC LIMIT 1"
    ))
    .fetch_optional(pool)
    .await?;
    return row.as_ref().map(stored).transpose();
}

/// Every profile, the active one first, then the most recently changed.
pub async fn list(pool: &SqlitePool) -> anyhow::Result<Vec<StoredProfile>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM profiles ORDER BY active DESC, updated_at DESC, id DESC"
    ))
    .fetch_all(pool)
    .await?;
    return rows.iter().map(stored).collect();
}

pub async fn get(pool: &SqlitePool, id: i64) -> anyhow::Result<Option<StoredProfile>> {
    let row = sqlx::query(&format!("SELECT {COLUMNS} FROM profiles WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    return row.as_ref().map(stored).transpose();
}

/// Renames a profile. The name doesn't affect ranking, so the matches stay fresh.
pub async fn rename(pool: &SqlitePool, id: i64, name: &str) -> anyhow::Result<bool> {
    let done = sqlx::query("UPDATE profiles SET name = ? WHERE id = ?")
        .bind(name)
        .bind(id)
        .execute(pool)
        .await?;
    return Ok(done.rows_affected() > 0);
}

/// Deletes a profile and (by cascade) its job matches. Deleting the active profile leaves
/// none active: the crawler goes back to neutral scoring.
pub async fn delete(pool: &SqlitePool, id: i64) -> anyhow::Result<bool> {
    let done = sqlx::query("DELETE FROM profiles WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    return Ok(done.rows_affected() > 0);
}

pub async fn find_by_hash(pool: &SqlitePool, hash: &str) -> anyhow::Result<Option<StoredProfile>> {
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM profiles WHERE cv_hash = ? ORDER BY id DESC LIMIT 1"
    ))
    .bind(hash)
    .fetch_optional(pool)
    .await?;
    return row.as_ref().map(stored).transpose();
}

/// Makes `id` the only active profile (its ranking is then stale until recomputed).
pub async fn activate(pool: &SqlitePool, id: i64) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE profiles SET active = 0 WHERE active = 1 AND id <> ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE profiles SET active = 1, updated_at = ? WHERE id = ?")
        .bind(now_ms())
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    return Ok(());
}

/// Adds a profile from a CV and makes it the active one.
pub async fn insert(
    pool: &SqlitePool,
    name: &str,
    source: &str,
    cv_hash: &str,
    cv_text: &str,
    extracted: &Profile,
) -> anyhow::Result<i64> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE profiles SET active = 0 WHERE active = 1")
        .execute(&mut *tx)
        .await?;
    let now = now_ms();
    let id = sqlx::query_scalar(
        "INSERT INTO profiles (name, active, cv_hash, cv_text, source, extracted, created_at, updated_at)
         VALUES (?, 1, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(name)
    .bind(cv_hash)
    .bind(cv_text)
    .bind(source)
    .bind(serde_json::to_string(extracted)?)
    .bind(now)
    .bind(now)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    return Ok(id);
}

pub async fn set_overrides(
    pool: &SqlitePool,
    id: i64,
    overrides: &Overrides,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE profiles SET overrides = ?, updated_at = ? WHERE id = ?")
        .bind(serde_json::to_string(overrides)?)
        .bind(now_ms())
        .bind(id)
        .execute(pool)
        .await?;
    return Ok(());
}

/// Records that job matches are up to date as of the profile version `updated_at`.
pub async fn mark_matched(pool: &SqlitePool, id: i64, updated_at: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE profiles SET matched_at = ? WHERE id = ?")
        .bind(updated_at)
        .bind(id)
        .execute(pool)
        .await?;
    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    #[tokio::test]
    async fn profiles_are_listed_renamed_and_deleted() {
        let (_dir, pool) = test_pool().await;
        let a = insert(&pool, "a", "parser", "h1", "text a", &extracted())
            .await
            .unwrap();
        let b = insert(&pool, "b", "parser", "h2", "text b", &extracted())
            .await
            .unwrap();
        // The newest is active and listed first.
        let all = list(&pool).await.unwrap();
        assert_eq!(
            all.iter().map(|p| (p.id, p.active)).collect::<Vec<_>>(),
            vec![(b, true), (a, false)]
        );
        assert!(rename(&pool, a, "older").await.unwrap());
        assert_eq!(get(&pool, a).await.unwrap().unwrap().name, "older");
        assert!(!rename(&pool, 999, "x").await.unwrap());
        assert!(delete(&pool, b).await.unwrap());
        assert!(active(&pool).await.unwrap().is_none());
        assert_eq!(list(&pool).await.unwrap().len(), 1);
        assert!(!delete(&pool, b).await.unwrap());
    }

    fn extracted() -> Profile {
        return Profile {
            titles: vec!["Backend Engineer".into()],
            seniority: Some("senior".into()),
            skills: vec![WeightedSkill {
                name: "rust".into(),
                weight: 0.9,
            }],
            ..Profile::default()
        };
    }

    #[test]
    fn overrides_win_field_by_field() {
        let merged = extracted().merged(&Overrides {
            titles: Some(vec!["Data Engineer".into()]),
            exclude: Some(vec!["crypto".into()]),
            ..Overrides::default()
        });
        assert_eq!(merged.titles, ["Data Engineer"]);
        assert_eq!(merged.exclude, ["crypto"]);
        assert_eq!(
            merged.seniority.as_deref(),
            Some("senior"),
            "untouched fields stay"
        );
        assert_eq!(merged.skills.len(), 1);
    }

    #[test]
    fn seniority_falls_back_to_years() {
        let mut p = Profile::default();
        assert_eq!(p.effective_seniority(), None);
        p.years_experience = Some(6.0);
        assert_eq!(p.effective_seniority(), Some("senior"));
        p.seniority = Some("lead".into());
        assert_eq!(p.effective_seniority(), Some("lead"));
    }

    #[tokio::test]
    async fn insert_activates_and_edits_survive() {
        let (_dir, pool) = test_pool().await;
        assert!(active(&pool).await.unwrap().is_none());
        let first = insert(&pool, "cv1", "parser", "h1", "text", &extracted())
            .await
            .unwrap();
        let second = insert(&pool, "cv2", "llm", "h2", "text", &extracted())
            .await
            .unwrap();
        assert_eq!(active(&pool).await.unwrap().unwrap().id, second);

        set_overrides(
            &pool,
            second,
            &Overrides {
                relocate: Some(true),
                ..Overrides::default()
            },
        )
        .await
        .unwrap();
        let p = active(&pool).await.unwrap().unwrap();
        assert!(p.merged().relocate);
        assert!(p.matches_stale());
        mark_matched(&pool, p.id, p.updated_at).await.unwrap();
        assert!(!active(&pool).await.unwrap().unwrap().matches_stale());

        // Going back to an earlier CV keeps that profile's own edits and activates it.
        assert_eq!(find_by_hash(&pool, "h1").await.unwrap().unwrap().id, first);
        activate(&pool, first).await.unwrap();
        assert_eq!(active(&pool).await.unwrap().unwrap().id, first);
        assert!(active(&pool).await.unwrap().unwrap().matches_stale());
    }
}
