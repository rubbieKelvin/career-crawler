//! The provider abstraction. One OpenAI-compatible implementation covers DeepSeek, OpenAI,
//! Groq, OpenRouter, Anthropic's compatible endpoint and local Ollama/llama.cpp servers;
//! the trait is there for providers that don't fit that mould.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// `system`, `user` or `assistant`.
    pub role: &'static str,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        return Self {
            role: "system",
            content: content.into(),
        };
    }

    pub fn user(content: impl Into<String>) -> Self {
        return Self {
            role: "user",
            content: content.into(),
        };
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        return Self {
            role: "assistant",
            content: content.into(),
        };
    }
}

/// A chat completion that must answer in JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatResponse {
    pub content: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },
    #[error("transport: {0}")]
    Transport(String),
    #[error("malformed response: {0}")]
    Malformed(String),
}

impl ProviderError {
    /// Worth trying again: rate limits, server errors and network failures.
    pub fn retryable(&self) -> bool {
        return match self {
            ProviderError::Http { status, .. } => *status == 429 || *status >= 500,
            ProviderError::Transport(_) => true,
            ProviderError::Malformed(_) => false,
        };
    }
}

pub trait Provider: Send + Sync {
    fn chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, ProviderError>>;
}

/// `POST {base_url}/chat/completions` with JSON mode on.
pub struct OpenAiCompatible {
    client: reqwest::Client,
    url: String,
    model: String,
    api_key: String,
}

impl std::fmt::Debug for OpenAiCompatible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        return f
            .debug_struct("OpenAiCompatible")
            .field("url", &self.url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .finish();
    }
}

#[derive(Deserialize)]
struct Completion {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Usage,
}

#[derive(Deserialize)]
struct Choice {
    message: ReplyMessage,
}

#[derive(Deserialize)]
struct ReplyMessage {
    content: Option<String>,
}

#[derive(Deserialize, Default)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

/// Error bodies are shown in logs; keep them short.
const MAX_ERROR_BODY: usize = 300;

impl OpenAiCompatible {
    pub fn new(
        base_url: &str,
        model: &str,
        api_key: &str,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder().timeout(timeout).build()?;
        return Ok(Self {
            client,
            url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            model: model.to_string(),
            api_key: api_key.to_string(),
        });
    }

    async fn send(&self, request: &ChatRequest) -> Result<ChatResponse, ProviderError> {
        let body = json!({
            "model": self.model,
            "messages": request
                .messages
                .iter()
                .map(|m| json!({ "role": m.role, "content": m.content }))
                .collect::<Vec<_>>(),
            "response_format": { "type": "json_object" },
            "temperature": 0,
        });
        let response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.without_url().to_string()))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ProviderError::Transport(e.without_url().to_string()))?;
        if !status.is_success() {
            let text = String::from_utf8_lossy(&bytes);
            return Err(ProviderError::Http {
                status: status.as_u16(),
                body: text.chars().take(MAX_ERROR_BODY).collect(),
            });
        }
        let completion: Completion =
            serde_json::from_slice(&bytes).map_err(|e| ProviderError::Malformed(e.to_string()))?;
        let content = completion
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or_else(|| ProviderError::Malformed("no message content".into()))?;
        return Ok(ChatResponse {
            content,
            tokens_in: completion.usage.prompt_tokens,
            tokens_out: completion.usage.completion_tokens,
        });
    }
}

impl Provider for OpenAiCompatible {
    fn chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, ProviderError>> {
        return Box::pin(self.send(request));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn request() -> ChatRequest {
        return ChatRequest {
            messages: vec![Message::system("sys"), Message::user("hi")],
        };
    }

    async fn provider(server: &MockServer) -> OpenAiCompatible {
        // A trailing slash on the base URL must not double up.
        return OpenAiCompatible::new(
            &format!("{}/", server.uri()),
            "m1",
            "sekrit",
            Duration::from_secs(5),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn posts_chat_completions_with_json_mode_and_reads_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer sekrit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{ "message": { "role": "assistant", "content": "{\"n\": 1}" } }],
                "usage": { "prompt_tokens": 12, "completion_tokens": 3 }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let reply = provider(&server).await.chat(&request()).await.unwrap();
        assert_eq!(
            reply,
            ChatResponse {
                content: "{\"n\": 1}".into(),
                tokens_in: 12,
                tokens_out: 3
            }
        );

        let sent: serde_json::Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(sent["model"], "m1");
        assert_eq!(sent["response_format"]["type"], "json_object");
        assert_eq!(
            sent["messages"][0],
            json!({ "role": "system", "content": "sys" })
        );
        assert_eq!(sent["messages"][1]["role"], "user");
    }

    #[tokio::test]
    async fn http_errors_carry_status_and_are_classified() {
        let server = MockServer::start().await;
        Mock::given(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&server)
            .await;
        let err = provider(&server).await.chat(&request()).await.unwrap_err();
        assert!(
            matches!(&err, ProviderError::Http { status: 429, body } if body == "rate limited")
        );
        assert!(err.retryable());
        assert!(
            !ProviderError::Http {
                status: 401,
                body: String::new()
            }
            .retryable()
        );
        assert!(
            ProviderError::Http {
                status: 503,
                body: String::new()
            }
            .retryable()
        );
    }

    #[tokio::test]
    async fn empty_or_garbled_bodies_are_malformed() {
        let server = MockServer::start().await;
        Mock::given(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "choices": [] })))
            .mount(&server)
            .await;
        let err = provider(&server).await.chat(&request()).await.unwrap_err();
        assert!(matches!(err, ProviderError::Malformed(_)));
        assert!(!err.retryable());
    }

    #[test]
    fn debug_output_hides_the_key() {
        let p = OpenAiCompatible::new("http://x", "m", "sekrit", Duration::from_secs(1)).unwrap();
        assert!(!format!("{p:?}").contains("sekrit"));
    }
}
