//! The optional LLM tier (see `brainstorms/10-llm.md`). `Llm` wraps a [`Provider`] with what
//! every task needs: JSON-only answers (one retry with the parse error attached), a cache
//! keyed by `(task, prompt version, model, input)`, a rolling daily token budget, bounded
//! concurrency, and an audit log of every provider call. All of it lives in `llm_calls`, so
//! restarts and re-crawls never pay twice.
//!
//! Every use of the LLM must be optional: callers get an `Err` (budget, provider, bad
//! output) and fall back to their heuristics.

pub mod provider;
pub mod tasks;
pub mod testing;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use areer_core::config::LlmConfig;
use areer_core::time::now_ms;
use serde::de::DeserializeOwned;
use sqlx::SqlitePool;
use tokio::sync::Semaphore;

use crate::provider::{
    ChatRequest, ChatResponse, Message, OpenAiCompatible, Provider, ProviderError,
};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// Extra tries after a transient provider failure (429, 5xx, network).
const PROVIDER_RETRIES: u32 = 2;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);

/// A versioned task prompt.
#[derive(Debug, Clone, Copy)]
pub struct Prompt {
    pub task: &'static str,
    pub version: u32,
    pub system: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Completion<T> {
    pub value: T,
    /// Served from `llm_calls`; no provider call was made.
    pub cached: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("daily token budget used up")]
    BudgetExhausted,
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
    #[error("the model's reply was not usable JSON: {0}")]
    InvalidOutput(String),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

/// Lock-free counters, cumulative for the process; the crawler's sampler reads them.
#[derive(Debug, Default)]
pub struct LlmStats {
    /// Requests sent to the provider (retries included).
    pub calls: AtomicU64,
    pub cache_hits: AtomicU64,
    /// Provider failures and unusable replies.
    pub errors: AtomicU64,
    pub tokens_in: AtomicU64,
    pub tokens_out: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LlmStatsSnapshot {
    pub calls: u64,
    pub cache_hits: u64,
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

impl LlmStats {
    pub fn snapshot(&self) -> LlmStatsSnapshot {
        return LlmStatsSnapshot {
            calls: self.calls.load(Relaxed),
            cache_hits: self.cache_hits.load(Relaxed),
            errors: self.errors.load(Relaxed),
            tokens_in: self.tokens_in.load(Relaxed),
            tokens_out: self.tokens_out.load(Relaxed),
        };
    }
}

pub struct Llm {
    provider: Arc<dyn Provider>,
    pool: SqlitePool,
    model: String,
    daily_token_budget: u64,
    input_price: f64,
    output_price: f64,
    slots: Semaphore,
    stats: Arc<LlmStats>,
    retry_delay: Duration,
}

impl std::fmt::Debug for Llm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        return f
            .debug_struct("Llm")
            .field("model", &self.model)
            .field("daily_token_budget", &self.daily_token_budget)
            .finish_non_exhaustive();
    }
}

impl Llm {
    pub fn new(config: &LlmConfig, pool: SqlitePool, provider: Arc<dyn Provider>) -> Self {
        return Self {
            provider,
            pool,
            model: config.model.clone(),
            daily_token_budget: config.daily_token_budget,
            input_price: config.input_price_per_mtok,
            output_price: config.output_price_per_mtok,
            slots: Semaphore::new(config.max_concurrency.max(1)),
            stats: Arc::new(LlmStats::default()),
            retry_delay: RETRY_BASE_DELAY,
        };
    }

    /// Base of the exponential backoff between retries of a transient provider failure.
    pub fn with_retry_delay(mut self, delay: Duration) -> Self {
        self.retry_delay = delay;
        return self;
    }

    /// The configured client, or `None` when the LLM is off or its API key isn't set (a
    /// warning explains which). The crawler carries on without it.
    pub fn from_config(config: &LlmConfig, pool: SqlitePool) -> anyhow::Result<Option<Arc<Self>>> {
        if !config.enabled {
            return Ok(None);
        }
        let key = std::env::var(&config.api_key_env).unwrap_or_default();
        if key.trim().is_empty() {
            tracing::warn!(
                env = %config.api_key_env,
                "llm.enabled is set but the API key variable is empty; continuing without the LLM"
            );
            return Ok(None);
        }
        let provider = OpenAiCompatible::new(
            &config.base_url,
            &config.model,
            key.trim(),
            Duration::from_secs(config.request_timeout_secs.max(1)),
        )?;
        tracing::info!(model = %config.model, base_url = %config.base_url, "LLM enabled");
        return Ok(Some(Arc::new(Self::new(config, pool, Arc::new(provider)))));
    }

    pub fn stats(&self) -> Arc<LlmStats> {
        return self.stats.clone();
    }

    pub fn model(&self) -> &str {
        return &self.model;
    }

    /// Runs `prompt` on `input` and parses the JSON answer as `T`.
    pub async fn complete_json<T: DeserializeOwned>(
        &self,
        prompt: &Prompt,
        input: &str,
    ) -> Result<Completion<T>, LlmError> {
        let key = self.cache_key(prompt, input);
        if let Some(output) = self.cached(&key).await? {
            // A cached reply that no longer fits `T` (the struct changed without a prompt
            // version bump) is simply asked again.
            if let Ok(value) = parse_json::<T>(&output) {
                self.stats.cache_hits.fetch_add(1, Relaxed);
                return Ok(Completion {
                    value,
                    cached: true,
                });
            }
        }

        let _slot = self
            .slots
            .acquire()
            .await
            .expect("the semaphore is never closed");
        let mut request = ChatRequest {
            messages: vec![Message::system(prompt.system), Message::user(input)],
        };
        let mut attempt = 0;
        loop {
            let started = Instant::now();
            let response = self.call(prompt, &key, &request).await?;
            let latency_ms = started.elapsed().as_millis() as i64;
            match parse_json::<T>(&response.content) {
                Ok(value) => {
                    self.log(prompt, &key, "ok", Some(&response), None, latency_ms)
                        .await?;
                    return Ok(Completion {
                        value,
                        cached: false,
                    });
                }
                Err(reason) => {
                    self.stats.errors.fetch_add(1, Relaxed);
                    self.log(
                        prompt,
                        &key,
                        "parse_error",
                        Some(&response),
                        Some(&reason),
                        latency_ms,
                    )
                    .await?;
                    attempt += 1;
                    if attempt > 1 {
                        return Err(LlmError::InvalidOutput(reason));
                    }
                    request.messages.push(Message::assistant(response.content));
                    request.messages.push(Message::user(format!(
                        "That reply could not be used ({reason}). Reply again with only the corrected JSON object."
                    )));
                }
            }
        }
    }

    /// One provider request, retried on transient failures. Enforces the token budget,
    /// counts tokens, and logs failures (successes are logged by the caller once parsed).
    async fn call(
        &self,
        prompt: &Prompt,
        key: &str,
        request: &ChatRequest,
    ) -> Result<ChatResponse, LlmError> {
        let mut retries = 0;
        loop {
            self.check_budget().await?;
            self.stats.calls.fetch_add(1, Relaxed);
            let started = Instant::now();
            match self.provider.chat(request).await {
                Ok(response) => {
                    self.stats.tokens_in.fetch_add(response.tokens_in, Relaxed);
                    self.stats
                        .tokens_out
                        .fetch_add(response.tokens_out, Relaxed);
                    return Ok(response);
                }
                Err(e) => {
                    self.stats.errors.fetch_add(1, Relaxed);
                    let latency_ms = started.elapsed().as_millis() as i64;
                    self.log(prompt, key, "error", None, Some(&e.to_string()), latency_ms)
                        .await?;
                    if e.retryable() && retries < PROVIDER_RETRIES {
                        tokio::time::sleep(self.retry_delay * 2u32.pow(retries)).await;
                        retries += 1;
                        continue;
                    }
                    return Err(e.into());
                }
            }
        }
    }

    fn cache_key(&self, prompt: &Prompt, input: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        for part in [prompt.task, &prompt.version.to_string(), &self.model, input] {
            // Length-prefixed so field boundaries can't blur into each other.
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        return hasher.finalize().to_hex().to_string();
    }

    async fn cached(&self, key: &str) -> Result<Option<String>, sqlx::Error> {
        return sqlx::query_scalar(
            "SELECT output FROM llm_calls WHERE cache_key = ? AND status = 'ok' ORDER BY id DESC LIMIT 1",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await;
    }

    /// Tokens spent (in + out) in the last 24 hours.
    pub async fn tokens_used_today(&self) -> Result<u64, sqlx::Error> {
        let used: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM llm_calls WHERE ts >= ?",
        )
        .bind(now_ms() - DAY_MS)
        .fetch_one(&self.pool)
        .await?;
        return Ok(used.max(0) as u64);
    }

    async fn check_budget(&self) -> Result<(), LlmError> {
        if self.daily_token_budget > 0 && self.tokens_used_today().await? >= self.daily_token_budget
        {
            return Err(LlmError::BudgetExhausted);
        }
        return Ok(());
    }

    async fn log(
        &self,
        prompt: &Prompt,
        key: &str,
        status: &str,
        response: Option<&ChatResponse>,
        error: Option<&str>,
        latency_ms: i64,
    ) -> Result<(), sqlx::Error> {
        let (tokens_in, tokens_out) = response.map_or((0, 0), |r| (r.tokens_in, r.tokens_out));
        let cost = (tokens_in as f64 * self.input_price + tokens_out as f64 * self.output_price)
            / 1_000_000.0;
        sqlx::query(
            "INSERT INTO llm_calls (ts, task, prompt_version, model, cache_key, status, output, error,
                                    tokens_in, tokens_out, cost_usd, latency_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(now_ms())
        .bind(prompt.task)
        .bind(prompt.version)
        .bind(&self.model)
        .bind(key)
        .bind(status)
        .bind(response.map(|r| r.content.as_str()))
        .bind(error)
        .bind(tokens_in as i64)
        .bind(tokens_out as i64)
        .bind(cost)
        .bind(latency_ms)
        .execute(&self.pool)
        .await?;
        return Ok(());
    }
}

/// Parses a model's reply as `T`, tolerating a Markdown code fence or prose around the
/// JSON object. Errors are short enough to send back to the model.
pub fn parse_json<T: DeserializeOwned>(reply: &str) -> Result<T, String> {
    let text = reply.trim();
    let text = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .map(|t| t.trim_end().trim_end_matches("```").trim())
        .unwrap_or(text);
    let first_try = serde_json::from_str::<T>(text);
    let error = match first_try {
        Ok(value) => return Ok(value),
        Err(e) => e,
    };
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}'))
        && start < end
        && let Ok(value) = serde_json::from_str::<T>(&text[start..=end])
    {
        return Ok(value);
    }
    return Err(error.to_string());
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use areer_core::db;
    use serde::Deserialize;

    use super::*;
    use crate::testing::FakeProvider;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Answer {
        n: i64,
    }

    const PROMPT: Prompt = Prompt {
        task: "test",
        version: 1,
        system: "Reply with JSON.",
    };

    async fn llm(
        provider: Arc<FakeProvider>,
        tweak: impl FnOnce(&mut LlmConfig),
    ) -> (tempfile::TempDir, Llm) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(&dir.path().join("t.db")).await.unwrap();
        let mut config = LlmConfig::default();
        tweak(&mut config);
        return (dir, Llm::new(&config, pool, provider));
    }

    async fn rows(llm: &Llm, status: &str) -> i64 {
        return sqlx::query_scalar("SELECT COUNT(*) FROM llm_calls WHERE status = ?")
            .bind(status)
            .fetch_one(&llm.pool)
            .await
            .unwrap();
    }

    #[test]
    fn parses_fenced_and_wrapped_json() {
        assert_eq!(parse_json::<Answer>(r#"{"n": 1}"#), Ok(Answer { n: 1 }));
        assert_eq!(
            parse_json::<Answer>("```json\n{\"n\": 2}\n```"),
            Ok(Answer { n: 2 })
        );
        assert_eq!(
            parse_json::<Answer>("Sure! Here you go: {\"n\": 3} Hope that helps."),
            Ok(Answer { n: 3 })
        );
        assert!(parse_json::<Answer>("no json here").is_err());
        assert!(parse_json::<Answer>(r#"{"m": 1}"#).is_err(), "wrong shape");
    }

    #[tokio::test]
    async fn answers_are_cached_by_task_version_model_and_input() {
        let provider = Arc::new(FakeProvider::replying(r#"{"n": 7}"#));
        let (_dir, llm) = llm(provider.clone(), |_| {}).await;

        let first = llm.complete_json::<Answer>(&PROMPT, "input").await.unwrap();
        assert_eq!((first.value, first.cached), (Answer { n: 7 }, false));
        let second = llm.complete_json::<Answer>(&PROMPT, "input").await.unwrap();
        assert_eq!((second.value, second.cached), (Answer { n: 7 }, true));
        assert_eq!(provider.call_count(), 1);

        // Any part of the key changing is a miss.
        llm.complete_json::<Answer>(&PROMPT, "other input")
            .await
            .unwrap();
        llm.complete_json::<Answer>(
            &Prompt {
                version: 2,
                ..PROMPT
            },
            "input",
        )
        .await
        .unwrap();
        assert_eq!(provider.call_count(), 3);

        let stats = llm.stats().snapshot();
        assert_eq!((stats.calls, stats.cache_hits, stats.errors), (3, 1, 0));
        assert_eq!((stats.tokens_in, stats.tokens_out), (30, 15));
        assert_eq!(rows(&llm, "ok").await, 3);
    }

    #[tokio::test]
    async fn the_cache_survives_a_restart() {
        let provider = Arc::new(FakeProvider::replying(r#"{"n": 1}"#));
        let (_dir, first) = llm(provider.clone(), |_| {}).await;
        first.complete_json::<Answer>(&PROMPT, "x").await.unwrap();
        let second = Llm::new(&LlmConfig::default(), first.pool.clone(), provider.clone());
        assert!(
            second
                .complete_json::<Answer>(&PROMPT, "x")
                .await
                .unwrap()
                .cached
        );
        assert_eq!(provider.call_count(), 1);
    }

    #[tokio::test]
    async fn a_bad_reply_is_retried_once_with_the_error_attached() {
        let calls = AtomicUsize::new(0);
        let provider = Arc::new(FakeProvider::new(move |_| {
            return Ok(if calls.fetch_add(1, Relaxed) == 0 {
                "not json".to_string()
            } else {
                r#"{"n": 5}"#.to_string()
            });
        }));
        let (_dir, llm) = llm(provider.clone(), |_| {}).await;
        let done = llm.complete_json::<Answer>(&PROMPT, "x").await.unwrap();
        assert_eq!(done.value, Answer { n: 5 });

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        let retry = &requests[1].messages;
        assert_eq!(retry.len(), 4, "system, input, bad reply, complaint");
        assert_eq!(retry[2], Message::assistant("not json"));
        assert!(retry[3].content.contains("could not be used"));
        assert_eq!(
            (rows(&llm, "parse_error").await, rows(&llm, "ok").await),
            (1, 1)
        );
    }

    #[tokio::test]
    async fn two_bad_replies_fail_and_are_never_cached() {
        let provider = Arc::new(FakeProvider::replying("still not json"));
        let (_dir, llm) = llm(provider.clone(), |_| {}).await;
        let err = llm.complete_json::<Answer>(&PROMPT, "x").await.unwrap_err();
        assert!(matches!(err, LlmError::InvalidOutput(_)), "{err}");
        assert_eq!(provider.call_count(), 2);
        // Asking again goes back to the provider.
        let _ = llm.complete_json::<Answer>(&PROMPT, "x").await;
        assert_eq!(provider.call_count(), 4);
        assert_eq!(rows(&llm, "ok").await, 0);
    }

    #[tokio::test]
    async fn transient_provider_errors_are_retried_with_backoff() {
        let calls = AtomicUsize::new(0);
        let provider = Arc::new(FakeProvider::new(move |_| {
            return if calls.fetch_add(1, Relaxed) < 2 {
                Err(ProviderError::Http {
                    status: 429,
                    body: "slow down".into(),
                })
            } else {
                Ok(r#"{"n": 9}"#.to_string())
            };
        }));
        let (_dir, llm) = llm(provider.clone(), |_| {}).await;
        let llm = llm.with_retry_delay(Duration::from_millis(1));
        assert_eq!(
            llm.complete_json::<Answer>(&PROMPT, "x")
                .await
                .unwrap()
                .value,
            Answer { n: 9 }
        );
        assert_eq!(provider.call_count(), 3);
        assert_eq!(rows(&llm, "error").await, 2);
    }

    #[tokio::test]
    async fn permanent_provider_errors_are_not_retried() {
        let provider = Arc::new(FakeProvider::new(|_| {
            return Err(ProviderError::Http {
                status: 401,
                body: "bad key".into(),
            });
        }));
        let (_dir, llm) = llm(provider.clone(), |_| {}).await;
        let err = llm.complete_json::<Answer>(&PROMPT, "x").await.unwrap_err();
        assert!(matches!(err, LlmError::Provider(_)));
        assert_eq!(provider.call_count(), 1);
    }

    #[tokio::test]
    async fn the_daily_token_budget_stops_calls_but_not_cache_hits() {
        let provider = Arc::new(FakeProvider::replying(r#"{"n": 1}"#));
        // Each call costs 15 tokens, so a budget of 20 allows exactly two.
        let (_dir, llm) = llm(provider.clone(), |c| c.daily_token_budget = 20).await;
        llm.complete_json::<Answer>(&PROMPT, "a").await.unwrap();
        llm.complete_json::<Answer>(&PROMPT, "b").await.unwrap();
        let err = llm.complete_json::<Answer>(&PROMPT, "c").await.unwrap_err();
        assert!(matches!(err, LlmError::BudgetExhausted));
        assert_eq!(provider.call_count(), 2);
        assert!(
            llm.complete_json::<Answer>(&PROMPT, "a")
                .await
                .unwrap()
                .cached
        );
    }

    #[tokio::test]
    async fn old_calls_fall_out_of_the_budget_window() {
        let provider = Arc::new(FakeProvider::replying(r#"{"n": 1}"#));
        let (_dir, llm) = llm(provider, |c| c.daily_token_budget = 10).await;
        llm.complete_json::<Answer>(&PROMPT, "a").await.unwrap();
        assert!(llm.complete_json::<Answer>(&PROMPT, "b").await.is_err());
        sqlx::query("UPDATE llm_calls SET ts = ts - ?")
            .bind(2 * DAY_MS)
            .execute(&llm.pool)
            .await
            .unwrap();
        llm.complete_json::<Answer>(&PROMPT, "b").await.unwrap();
    }

    #[tokio::test]
    async fn cost_is_estimated_from_the_configured_prices() {
        let provider = Arc::new(FakeProvider::replying(r#"{"n": 1}"#));
        let (_dir, llm) = llm(provider, |c| {
            c.input_price_per_mtok = 1_000_000.0;
            c.output_price_per_mtok = 2_000_000.0;
        })
        .await;
        llm.complete_json::<Answer>(&PROMPT, "a").await.unwrap();
        let cost: f64 = sqlx::query_scalar("SELECT cost_usd FROM llm_calls")
            .fetch_one(&llm.pool)
            .await
            .unwrap();
        assert_eq!(cost, 10.0 + 10.0);
    }

    #[tokio::test]
    async fn missing_api_key_disables_the_llm_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(&dir.path().join("t.db")).await.unwrap();
        let mut config = LlmConfig::default();
        assert!(
            Llm::from_config(&config, pool.clone()).unwrap().is_none(),
            "disabled"
        );
        config.enabled = true;
        config.api_key_env = "AREER_TEST_KEY_THAT_IS_NOT_SET".into();
        assert!(Llm::from_config(&config, pool).unwrap().is_none(), "no key");
    }
}
