//! CV → `Profile` (see `brainstorms/12-cv-profile.md`): validate and read an uploaded CV,
//! build the profile with the LLM when one is allowed to see it (else, or on failure, the
//! parser), and store it as the active profile. Used by the crawler (`crawler profile`) and
//! the UI (`POST /api/profile/cv`).

pub mod llm;
pub mod parse;
pub mod text;

use std::path::Path;

use career_core::profile::{self, Profile};
use career_llm::Llm;
use sqlx::SqlitePool;

pub use text::CvError;

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error(transparent)]
    Cv(#[from] CvError),
    #[error("database: {0}")]
    Db(#[from] anyhow::Error),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ingested {
    pub profile_id: i64,
    /// `llm` or `parser`, for a new profile; the stored source for a CV seen before.
    pub source: String,
    /// The same CV was uploaded before: its profile (and the edits made to it) came back.
    pub reused: bool,
}

fn current_year() -> i32 {
    // Mean Gregorian year: exact enough for "is this date range in the future".
    return 1970 + (career_core::time::now_ms() / 31_556_952_000) as i32;
}

/// The profile a CV's text gives, and which path produced it. `llm = None` (the LLM is
/// off, or `llm.send_cv` is false) means the parser.
pub async fn build_profile(cv_text: &str, llm: Option<&Llm>) -> (Profile, &'static str) {
    let parsed = parse::profile_from_text(cv_text, current_year());
    let Some(llm) = llm else {
        return (parsed, "parser");
    };
    return match llm::extract(llm, cv_text).await {
        Ok(answer) => (llm::into_profile(&answer, &parsed), "llm"),
        Err(e) => {
            tracing::warn!(error = %e, "LLM CV extraction failed; using the parser");
            (parsed, "parser")
        }
    };
}

/// Validates, reads and stores a CV as the active profile. Uploading the same CV again
/// re-activates its profile instead of making a copy, so the user's edits aren't lost.
pub async fn ingest(
    pool: &SqlitePool,
    llm: Option<&Llm>,
    bytes: &[u8],
    filename: &str,
) -> Result<Ingested, IngestError> {
    let text = text::extract(bytes, filename)?;
    let hash = blake3::hash(text.as_bytes()).to_hex().to_string();
    if let Some(existing) = profile::find_by_hash(pool, &hash).await? {
        profile::activate(pool, existing.id).await?;
        return Ok(Ingested {
            profile_id: existing.id,
            source: existing.source,
            reused: true,
        });
    }
    let (built, source) = build_profile(&text, llm).await;
    let name = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("cv");
    let id = profile::insert(pool, name, source, &hash, &text, &built).await?;
    return Ok(Ingested {
        profile_id: id,
        source: source.to_string(),
        reused: false,
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use career_core::config::LlmConfig;
    use career_core::db;
    use career_llm::testing::FakeProvider;

    use super::*;
    use crate::parse::tests::CV;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(&dir.path().join("t.db")).await.unwrap();
        return (dir, pool);
    }

    #[tokio::test]
    async fn parser_path_stores_the_active_profile() {
        let (_dir, pool) = pool().await;
        let done = ingest(&pool, None, CV.as_bytes(), "jane_cv.md")
            .await
            .unwrap();
        assert_eq!((done.source.as_str(), done.reused), ("parser", false));
        let stored = profile::active(&pool).await.unwrap().unwrap();
        assert_eq!(stored.id, done.profile_id);
        assert_eq!(stored.name, "jane_cv");
        assert!(stored.extracted.skills.iter().any(|s| s.name == "rust"));
        assert!(stored.matches_stale());
    }

    #[tokio::test]
    async fn the_same_cv_reuses_its_profile_and_keeps_the_edits() {
        let (_dir, pool) = pool().await;
        let first = ingest(&pool, None, CV.as_bytes(), "a.md").await.unwrap();
        profile::set_overrides(
            &pool,
            first.profile_id,
            &profile::Overrides {
                relocate: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // Another CV becomes active, then the first comes back.
        ingest(
            &pool,
            None,
            b"Someone Else\nData Analyst with several years of SQL and Excel work",
            "b.txt",
        )
        .await
        .unwrap();
        let again = ingest(&pool, None, CV.as_bytes(), "renamed.md")
            .await
            .unwrap();
        assert!(again.reused);
        assert_eq!(again.profile_id, first.profile_id);
        let stored = profile::active(&pool).await.unwrap().unwrap();
        assert!(stored.merged().relocate);
        let profiles: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM profiles")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(profiles, 2);
    }

    #[tokio::test]
    async fn invalid_uploads_store_nothing() {
        let (_dir, pool) = pool().await;
        let err = ingest(&pool, None, b"MZ\x90\x00binary", "cv.txt")
            .await
            .unwrap_err();
        assert!(matches!(err, IngestError::Cv(CvError::NotText)));
        assert!(profile::active(&pool).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn llm_path_is_used_when_allowed_and_falls_back_when_it_fails() {
        let (_dir, pool) = pool().await;
        let provider = Arc::new(FakeProvider::replying(
            r#"{"titles": ["Platform Engineer"], "seniority": "lead", "skills": [{"name": "Go", "weight": 0.9}],
                "locations": ["Accra, Ghana"]}"#,
        ));
        let llm = Llm::new(&LlmConfig::default(), pool.clone(), provider.clone());
        let done = ingest(&pool, Some(&llm), CV.as_bytes(), "cv.md")
            .await
            .unwrap();
        assert_eq!(done.source, "llm");
        let p = profile::active(&pool).await.unwrap().unwrap().extracted;
        assert_eq!(p.titles, ["Platform Engineer"]);
        assert_eq!(p.locations[0].name, "Accra, GH");
        assert_eq!(
            p.industries,
            ["fintech", "e-commerce"],
            "gaps come from the parser"
        );
        assert!(
            provider.requests()[0].messages[1]
                .content
                .contains("Jane Doe")
        );

        let broken = Arc::new(FakeProvider::replying("nope"));
        let llm = Llm::new(&LlmConfig::default(), pool.clone(), broken);
        let done = ingest(
            &pool,
            Some(&llm),
            b"Another Person\nDesigner with a lot of Figma experience here.",
            "x.txt",
        )
        .await
        .unwrap();
        assert_eq!(done.source, "parser");
    }
}
