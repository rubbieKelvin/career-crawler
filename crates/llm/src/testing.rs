//! A scripted provider for tests, in this crate and in the crawler's.

use std::sync::Mutex;

use crate::provider::{BoxFuture, ChatRequest, ChatResponse, Provider, ProviderError};

type Handler = Box<dyn Fn(&ChatRequest) -> Result<String, ProviderError> + Send + Sync>;

/// Answers each request by calling `handler` with it; every reply costs 10 tokens in and
/// 5 out. Requests are recorded for assertions.
pub struct FakeProvider {
    handler: Handler,
    requests: Mutex<Vec<ChatRequest>>,
}

impl FakeProvider {
    pub fn new(
        handler: impl Fn(&ChatRequest) -> Result<String, ProviderError> + Send + Sync + 'static,
    ) -> Self {
        return Self {
            handler: Box::new(handler),
            requests: Mutex::new(Vec::new()),
        };
    }

    /// Always replies with `content`.
    pub fn replying(content: &str) -> Self {
        let content = content.to_string();
        return Self::new(move |_| Ok(content.clone()));
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
        return self.requests.lock().unwrap().clone();
    }

    pub fn call_count(&self) -> usize {
        return self.requests.lock().unwrap().len();
    }
}

impl Provider for FakeProvider {
    fn chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, ProviderError>> {
        self.requests.lock().unwrap().push(request.clone());
        let reply = (self.handler)(request);
        return Box::pin(async move {
            return reply.map(|content| ChatResponse {
                content,
                tokens_in: 10,
                tokens_out: 5,
            });
        });
    }
}
