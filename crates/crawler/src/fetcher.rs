//! Single HTTP request, no redirect following (the caller decides, so robots.txt and
//! politeness apply to every hop). Automatic decompression is off in reqwest so we can
//! count real wire bytes, then decompress here, capped both ways (`max_body_bytes` for
//! pages, `max_resource_bytes` for everything fetched with `Want::Any`).

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use bytes::Bytes;
use areer_core::config::CrawlerConfig;
use reqwest::header::{self, HeaderMap};
use reqwest::{StatusCode, redirect};
use url::Url;

use crate::metrics::Metrics;

/// Rough per-request overhead of the headers reqwest adds on top of ours.
const TX_OVERHEAD_BYTES: u64 = 64;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("timed out")]
    Timeout,
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("body exceeds {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("could not decode body: {0}")]
    Decode(String),
    #[error("request failed: {0}")]
    Other(String),
}

impl FetchError {
    /// Stable label for metrics and stored errors.
    pub fn kind(&self) -> &'static str {
        return match self {
            FetchError::Timeout => "timeout",
            FetchError::Connect(_) => "connect",
            FetchError::TooLarge { .. } => "too_large",
            FetchError::Decode(_) => "decode",
            FetchError::Other(_) => "other",
        };
    }

    fn from_reqwest(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            return FetchError::Timeout;
        }
        if e.is_connect() {
            return FetchError::Connect(error_chain(&e));
        }
        return FetchError::Other(error_chain(&e));
    }
}

/// reqwest's top-level message is vague ("error sending request"); include the causes.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        source = s.source();
    }
    return msg;
}

/// Which bodies are worth downloading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// HTML/XHTML only; other content types are not downloaded.
    Html,
    /// Anything (robots.txt is often served with the wrong type).
    Any,
}

#[derive(Debug)]
pub struct Response {
    pub url: Url,
    pub status: StatusCode,
    pub headers: HeaderMap,
    /// Decompressed body. Empty for redirects and skipped content types.
    pub body: Bytes,
    /// Body skipped because of its content type.
    pub skipped: bool,
    pub bytes_wire: u64,
    pub elapsed: Duration,
}

impl Response {
    pub fn content_type(&self) -> Option<&str> {
        return self.headers.get(header::CONTENT_TYPE)?.to_str().ok();
    }

    /// `Location` resolved against the request URL, for 3xx responses.
    pub fn redirect_target(&self) -> Option<Url> {
        if !self.status.is_redirection() {
            return None;
        }
        let location = self.headers.get(header::LOCATION)?.to_str().ok()?;
        return self.url.join(location).ok();
    }
}

pub fn is_html(content_type: Option<&str>) -> bool {
    let Some(ct) = content_type else {
        // Missing header: let the parser try.
        return true;
    };
    let mime = ct
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    return mime == "text/html" || mime == "application/xhtml+xml";
}

pub struct Fetcher {
    client: reqwest::Client,
    max_body_bytes: u64,
    max_resource_bytes: u64,
    user_agent: String,
    metrics: Arc<Metrics>,
}

impl Fetcher {
    pub fn new(config: &CrawlerConfig, metrics: Arc<Metrics>) -> anyhow::Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, "gzip, deflate, br".parse()?);
        headers.insert(
            header::ACCEPT,
            "text/html,application/xhtml+xml;q=0.9,*/*;q=0.5".parse()?,
        );
        let client = reqwest::Client::builder()
            .user_agent(&config.user_agent)
            .default_headers(headers)
            .redirect(redirect::Policy::none())
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
            .build()?;
        return Ok(Self {
            client,
            max_body_bytes: config.max_body_bytes,
            max_resource_bytes: config.max_resource_bytes,
            user_agent: config.user_agent.clone(),
            metrics,
        });
    }

    pub async fn fetch(&self, url: &Url, want: Want) -> Result<Response, FetchError> {
        let result = self.fetch_inner(url, want).await;
        if result.is_err() {
            self.metrics.fetch_errors.fetch_add(1, Relaxed);
        }
        return result;
    }

    async fn fetch_inner(&self, url: &Url, want: Want) -> Result<Response, FetchError> {
        let started = Instant::now();
        let m = &self.metrics;
        m.requests.fetch_add(1, Relaxed);
        m.bytes_tx.fetch_add(
            url.as_str().len() as u64 + self.user_agent.len() as u64 + TX_OVERHEAD_BYTES,
            Relaxed,
        );

        let mut resp = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(FetchError::from_reqwest)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        m.record_status(status.as_u16());
        let header_bytes: u64 = headers
            .iter()
            .map(|(k, v)| (k.as_str().len() + v.len() + 4) as u64)
            .sum();
        m.bytes_rx_wire.fetch_add(header_bytes, Relaxed);

        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        let skip_body = status.is_redirection() || (want == Want::Html && !is_html(content_type));
        if skip_body {
            return Ok(Response {
                url: url.clone(),
                status,
                headers,
                body: Bytes::new(),
                skipped: !status.is_redirection(),
                bytes_wire: header_bytes,
                elapsed: started.elapsed(),
            });
        }

        let limit = match want {
            Want::Html => self.max_body_bytes,
            Want::Any => self.max_resource_bytes,
        };
        if resp.content_length().is_some_and(|len| len > limit) {
            return Err(FetchError::TooLarge { limit });
        }
        let mut wire = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(FetchError::from_reqwest)? {
            m.bytes_rx_wire.fetch_add(chunk.len() as u64, Relaxed);
            wire.extend_from_slice(&chunk);
            if wire.len() as u64 > limit {
                m.bytes_wasted.fetch_add(wire.len() as u64, Relaxed);
                return Err(FetchError::TooLarge { limit });
            }
        }
        let wire_len = wire.len() as u64;

        let encoding = headers
            .get(header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok());
        let body = match decompress(encoding, wire, limit) {
            Ok(body) => body,
            Err(e) => {
                m.bytes_wasted.fetch_add(wire_len, Relaxed);
                return Err(e);
            }
        };
        m.bytes_rx_body.fetch_add(body.len() as u64, Relaxed);

        return Ok(Response {
            url: url.clone(),
            status,
            headers,
            body: Bytes::from(body),
            skipped: false,
            bytes_wire: header_bytes + wire_len,
            elapsed: started.elapsed(),
        });
    }
}

/// Decodes a `Content-Encoding`. Output is capped at `limit` bytes to stop decompression bombs.
pub fn decompress(
    encoding: Option<&str>,
    wire: Vec<u8>,
    limit: u64,
) -> Result<Vec<u8>, FetchError> {
    let encoding = encoding.unwrap_or("identity").trim().to_ascii_lowercase();
    let reader: Box<dyn Read> = match encoding.as_str() {
        "" | "identity" => return Ok(wire),
        "gzip" | "x-gzip" => Box::new(flate2::read::MultiGzDecoder::new(std::io::Cursor::new(
            wire,
        ))),
        // "deflate" is meant to be zlib-wrapped, but some servers send raw deflate.
        "deflate" if wire.first().is_some_and(|b| b & 0x0f == 8) => {
            Box::new(flate2::read::ZlibDecoder::new(std::io::Cursor::new(wire)))
        }
        "deflate" => Box::new(flate2::read::DeflateDecoder::new(std::io::Cursor::new(
            wire,
        ))),
        "br" => Box::new(brotli::Decompressor::new(std::io::Cursor::new(wire), 4096)),
        other => {
            return Err(FetchError::Decode(format!(
                "unsupported content-encoding {other:?}"
            )));
        }
    };
    let mut out = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut out)
        .map_err(|e| FetchError::Decode(e.to_string()))?;
    if out.len() as u64 > limit {
        return Err(FetchError::TooLarge { limit });
    }
    return Ok(out);
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(data).unwrap();
        return enc.finish().unwrap();
    }

    fn fetcher(max_body_bytes: u64) -> (Fetcher, Arc<Metrics>) {
        let metrics = Arc::new(Metrics::default());
        let config = CrawlerConfig {
            max_body_bytes,
            ..CrawlerConfig::default()
        };
        return (Fetcher::new(&config, metrics.clone()).unwrap(), metrics);
    }

    #[test]
    fn html_content_types() {
        assert!(is_html(Some("text/html; charset=utf-8")));
        assert!(is_html(Some("Application/XHTML+XML")));
        assert!(is_html(None));
        assert!(!is_html(Some("application/pdf")));
    }

    #[test]
    fn decompress_formats_and_limits() {
        let text = b"hello hello hello hello".to_vec();
        assert_eq!(decompress(None, text.clone(), 100).unwrap(), text);
        assert_eq!(decompress(Some("gzip"), gzip(&text), 100).unwrap(), text);

        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&text).unwrap();
        assert_eq!(
            decompress(Some("deflate"), z.finish().unwrap(), 100).unwrap(),
            text
        );

        let mut br = Vec::new();
        brotli::CompressorWriter::new(&mut br, 4096, 5, 22)
            .write_all(&text)
            .unwrap();
        assert_eq!(decompress(Some("br"), br, 100).unwrap(), text);

        let bomb = gzip(&vec![b'a'; 10_000]);
        assert!(matches!(
            decompress(Some("gzip"), bomb, 1000),
            Err(FetchError::TooLarge { .. })
        ));
        assert!(matches!(
            decompress(Some("zstd"), text, 100),
            Err(FetchError::Decode(_))
        ));
    }

    #[tokio::test]
    async fn counts_wire_bytes_before_decompression() {
        let server = MockServer::start().await;
        let html = "<html><body>".to_string() + &"job ".repeat(2000) + "</body></html>";
        let compressed = gzip(html.as_bytes());
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html; charset=utf-8")
                    .insert_header("content-encoding", "gzip")
                    .set_body_bytes(compressed.clone()),
            )
            .mount(&server)
            .await;

        let (f, metrics) = fetcher(1 << 20);
        let resp = f
            .fetch(&Url::parse(&server.uri()).unwrap(), Want::Html)
            .await
            .unwrap();
        assert_eq!(resp.body, html.as_bytes());
        let snap = metrics.snapshot();
        assert_eq!(snap.bytes_rx_body, html.len() as u64);
        assert!(snap.bytes_rx_wire >= compressed.len() as u64);
        assert!(
            snap.bytes_rx_wire < html.len() as u64,
            "wire bytes should reflect compression"
        );
        assert_eq!(snap.status_classes[1], 1);
    }

    #[tokio::test]
    async fn skips_non_html_and_reports_redirects() {
        let server = MockServer::start().await;
        Mock::given(path("/file.pdf"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/pdf")
                    .set_body_bytes(vec![0u8; 5000]),
            )
            .mount(&server)
            .await;
        Mock::given(path("/old"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", "/new?utm_source=x"))
            .mount(&server)
            .await;

        let (f, _) = fetcher(1 << 20);
        let base = Url::parse(&server.uri()).unwrap();
        let pdf = f
            .fetch(&base.join("/file.pdf").unwrap(), Want::Html)
            .await
            .unwrap();
        assert!(pdf.skipped);
        assert!(pdf.body.is_empty());

        let old = f
            .fetch(&base.join("/old").unwrap(), Want::Html)
            .await
            .unwrap();
        assert_eq!(old.status, StatusCode::MOVED_PERMANENTLY);
        assert_eq!(old.redirect_target().unwrap().path(), "/new");
    }

    #[tokio::test]
    async fn rejects_oversized_bodies() {
        let server = MockServer::start().await;
        Mock::given(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("x".repeat(10_000), "text/html"))
            .mount(&server)
            .await;
        let (f, metrics) = fetcher(1000);
        let url = Url::parse(&server.uri()).unwrap().join("/big").unwrap();
        let err = f.fetch(&url, Want::Html).await.unwrap_err();
        assert_eq!(err.kind(), "too_large");
        assert_eq!(metrics.snapshot().fetch_errors, 1);
    }
}
