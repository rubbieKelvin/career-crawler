//! Asks the LLM about gray-zone domains: a real homepage whose heuristic company score is
//! neither high nor low enough to decide. The input is a compact text digest of the page
//! (never raw HTML), and the model's answer is applied by `store::record_verdict`.

use areer_llm::tasks::{CLASSIFY_DOMAIN, DomainVerdict};
use areer_llm::{Llm, LlmError};

use crate::store::ClassifyRequest;

/// Anchors listed as the site's navigation.
const MAX_NAV_LINKS: usize = 40;
const MAX_ANCHOR_CHARS: usize = 60;
const MAX_TEXT_CHARS: usize = 2_500;
const MAX_FOOTER_CHARS: usize = 500;

/// The digest sent to the model.
pub fn evidence(request: &ClassifyRequest) -> String {
    let page = &request.parsed;
    let nav: Vec<String> = page
        .links
        .iter()
        .map(|l| l.text.trim())
        .filter(|t| !t.is_empty())
        .map(|t| t.chars().take(MAX_ANCHOR_CHARS).collect::<String>())
        .fold(Vec::new(), |mut seen: Vec<String>, t| {
            if !seen.contains(&t) {
                seen.push(t);
            }
            return seen;
        })
        .into_iter()
        .take(MAX_NAV_LINKS)
        .collect();
    let mut out = String::new();
    out.push_str(&format!("domain: {}\n", request.domain));
    out.push_str(&format!("url: {}\n", request.url));
    out.push_str(&format!(
        "title: {}\n",
        page.title.as_deref().unwrap_or("-")
    ));
    if let Some(name) = &page.og_site_name {
        out.push_str(&format!("site name: {name}\n"));
    }
    out.push_str(&format!(
        "heuristic company score: {:.2} (signals: {})\n",
        request.score,
        if request.signals.is_empty() {
            "none".to_string()
        } else {
            request.signals.join(", ")
        }
    ));
    out.push_str(&format!("link texts: {}\n", nav.join(" | ")));
    out.push_str(&format!(
        "footer: {}\n",
        page.footer_text
            .chars()
            .take(MAX_FOOTER_CHARS)
            .collect::<String>()
    ));
    out.push_str(&format!(
        "page text: {}\n",
        page.text.chars().take(MAX_TEXT_CHARS).collect::<String>()
    ));
    return out;
}

/// The model's opinion of the domain. Errors (budget, provider, unusable JSON) mean "no
/// opinion": the caller leaves the domain as the heuristics left it.
pub async fn ask(llm: &Llm, request: &ClassifyRequest) -> Result<DomainVerdict, LlmError> {
    let done = llm
        .complete_json::<DomainVerdict>(&CLASSIFY_DOMAIN, &evidence(request))
        .await?;
    return Ok(done.value);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use areer_core::config::LlmConfig;
    use areer_core::db;
    use areer_llm::testing::FakeProvider;
    use url::Url;

    use super::*;
    use crate::parse;

    pub(crate) fn request(html: &str) -> ClassifyRequest {
        let url = Url::parse("https://acme.example/").unwrap();
        return ClassifyRequest {
            domain: "acme.example".into(),
            domain_id: 1,
            page_id: 1,
            url: url.clone(),
            depth: 0,
            parsed: parse::parse_html(&url, html),
            score: 0.45,
            signals: vec!["https", "about_link"],
        };
    }

    #[test]
    fn the_digest_is_text_only_and_bounded() {
        let html = format!(
            "<title>Acme</title><script>var secret = 1</script><nav><a href='/a'>Products</a>\
             <a href='/b'>Products</a><a href='/c'>Pricing</a></nav><p>{}</p>",
            "word ".repeat(5_000)
        );
        let text = evidence(&request(&html));
        assert!(text.contains("title: Acme"));
        assert!(text.contains("heuristic company score: 0.45 (signals: https, about_link)"));
        assert!(
            text.contains("link texts: Products | Pricing"),
            "deduplicated: {text}"
        );
        assert!(!text.contains("secret"), "scripts are not evidence");
        assert!(!text.contains('<'), "no markup");
        assert!(text.len() < 4_000, "{}", text.len());
    }

    #[tokio::test]
    async fn asks_the_model_and_parses_the_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(&dir.path().join("t.db")).await.unwrap();
        let provider = Arc::new(FakeProvider::replying(
            r#"{"is_company": true, "company_name": "Acme", "industry": "Logistics",
                "hq_country": "NG", "confidence": 0.9, "reason": "sells freight services"}"#,
        ));
        let llm = Llm::new(&LlmConfig::default(), pool, provider.clone());
        let verdict = ask(&llm, &request("<title>Acme</title>")).await.unwrap();
        assert!(verdict.is_company);
        assert_eq!(verdict.company_name.as_deref(), Some("Acme"));
        let sent = &provider.requests()[0].messages;
        assert!(
            sent[0].content.contains("untrusted"),
            "the system prompt warns about injection"
        );
        assert!(sent[1].content.starts_with("domain: acme.example"));
    }
}
