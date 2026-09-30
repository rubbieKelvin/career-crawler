//! Seed list loading: one URL per line, `#` comments, blank lines ignored.
//! A bare host like `example.com` is treated as `https://example.com/`.

use std::path::Path;

use anyhow::Context;
use sqlx::SqlitePool;
use url::Url;

use crate::time::now_ms;
use crate::urls;

/// Frontier score for seeds, so they are crawled before anything discovered.
pub const SEED_SCORE: f64 = 100.0;

#[derive(Debug, Clone, PartialEq)]
pub struct InvalidSeed {
    pub line: usize,
    pub text: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct ParsedSeeds {
    pub urls: Vec<Url>,
    pub invalid: Vec<InvalidSeed>,
}

pub fn parse(text: &str) -> ParsedSeeds {
    let mut out = ParsedSeeds::default();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.split_once('#').map_or(raw, |(before, _)| before).trim();
        if line.is_empty() {
            continue;
        }
        match parse_line(line) {
            Ok(url) => out.urls.push(url),
            Err(reason) => out.invalid.push(InvalidSeed {
                line: i + 1,
                text: line.to_string(),
                reason,
            }),
        }
    }
    return out;
}

fn parse_line(line: &str) -> Result<Url, String> {
    let with_scheme = if line.contains("://") {
        line.to_string()
    } else {
        format!("https://{line}")
    };
    let url = Url::parse(&with_scheme).map_err(|e| e.to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("unsupported scheme {:?}", url.scheme()));
    }
    if url.host_str().is_none_or(|h| !h.contains('.')) {
        return Err("host must be a domain name".into());
    }
    return Ok(urls::normalize(&url));
}

pub fn read(path: &Path) -> anyhow::Result<ParsedSeeds> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading seeds {}", path.display()))?;
    return Ok(parse(&text));
}

/// Inserts seeds into the frontier. URLs already present are left untouched, so reloading
/// the same file is a no-op. Returns how many were newly enqueued.
pub async fn enqueue(pool: &SqlitePool, urls: &[Url]) -> anyhow::Result<u64> {
    let now = now_ms();
    let mut tx = pool.begin().await?;
    let mut inserted = 0;
    for url in urls {
        inserted += sqlx::query(
            "INSERT OR IGNORE INTO frontier (url, score, depth, reason, state, enqueued_at)
             VALUES (?, ?, 0, 'seed', 'queued', ?)",
        )
        .bind(url.as_str())
        .bind(SEED_SCORE)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    return Ok(inserted);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    #[test]
    fn parses_comments_blanks_and_bare_hosts() {
        let parsed = parse(
            "# portfolio pages\n\
             https://www.ycombinator.com/companies\n\
             \n\
             paystack.com   # bare host\n\
             http://example.org/a#frag\n",
        );
        let urls: Vec<&str> = parsed.urls.iter().map(Url::as_str).collect();
        assert_eq!(
            urls,
            [
                "https://www.ycombinator.com/companies",
                "https://paystack.com/",
                "http://example.org/a"
            ]
        );
        assert!(parsed.invalid.is_empty());
    }

    #[test]
    fn reports_invalid_lines_with_line_numbers() {
        let parsed = parse("ok.com\nftp://files.example.com\nlocalhost\nhttps://\n");
        assert_eq!(parsed.urls.len(), 1);
        assert_eq!(
            parsed.invalid.iter().map(|s| s.line).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
    }

    #[tokio::test]
    async fn enqueue_is_idempotent() {
        let (_dir, pool) = test_pool().await;
        let urls = parse("a.com\nb.com\na.com\n").urls;
        assert_eq!(enqueue(&pool, &urls).await.unwrap(), 2);
        assert_eq!(enqueue(&pool, &urls).await.unwrap(), 0);
        let (depth, reason, score): (i64, String, f64) = sqlx::query_as(
            "SELECT depth, reason, score FROM frontier WHERE url = 'https://a.com/'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((depth, reason.as_str(), score), (0, "seed", SEED_SCORE));
    }
}
