//! End-to-end tests: a real server on a random port over a seeded temp database.

use std::sync::Arc;
use std::time::Duration;

use career_core::config::LlmConfig;
use career_core::db;
use career_core::events::{self, Event};
use career_core::samples::{self, Sample};
use career_core::time::now_ms;
use career_llm::Llm;
use career_llm::testing::FakeProvider;
use futures_util::StreamExt;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::{router, start};

struct Server {
    base: String,
    pool: SqlitePool,
    _dir: tempfile::TempDir,
}

async fn server() -> Server {
    return server_with(|_| {}).await;
}

/// A server whose `AppState` the test has already adjusted (an LLM client, say).
async fn server_with(setup: impl FnOnce(&mut crate::AppState)) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::open(&dir.path().join("t.db")).await.unwrap();
    seed(&pool).await;
    let mut state = start(pool.clone(), Duration::from_millis(30));
    setup(&mut state);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    return Server {
        base,
        pool,
        _dir: dir,
    };
}

async fn seed(pool: &SqlitePool) {
    for sql in [
        "INSERT INTO domains (id, host, name, status, company_score, score_reasons, careers_url, first_seen)
         VALUES (1, 'acme.com', 'Acme', 'company', 0.9, '{\"signals\":[\"org_json_ld\"]}', 'https://acme.com/careers', 0),
                (2, 'vc.com', 'VC', 'company', 0.8, NULL, NULL, 0),
                (3, 'linked.com', NULL, 'discovered', NULL, NULL, NULL, 0)",
        "INSERT INTO pages (url, domain_id, kind, fetched_at) VALUES
           ('https://acme.com/', 1, 'home', 1), ('https://acme.com/careers', 1, 'careers', 2), ('https://vc.com/', 2, 'home', 3)",
        "INSERT INTO edges (src_domain_id, dst_domain_id, weight, first_seen) VALUES (2, 1, 3, 0), (2, 3, 1, 0)",
        "INSERT INTO jobs (url, domain_id, title, source, first_seen, last_seen) VALUES
           ('https://acme.com/careers/1', 1, 'Engineer', 'jsonld', 0, 0),
           ('https://acme.com/careers/2', 1, 'Closed role', 'jsonld', 0, 0)",
        "UPDATE jobs SET closed_at = 1 WHERE title = 'Closed role'",
    ] {
        sqlx::query(sql).execute(pool).await.unwrap();
    }
}

async fn get(s: &Server, path: &str) -> (u16, Value) {
    let resp = reqwest::get(format!("{}{path}", s.base)).await.unwrap();
    let status = resp.status().as_u16();
    return (
        status,
        serde_json::from_str(&resp.text().await.unwrap()).unwrap_or(Value::Null),
    );
}

async fn post(s: &Server, path: &str) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{}{path}", s.base))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    return (
        status,
        serde_json::from_str(&resp.text().await.unwrap()).unwrap(),
    );
}

fn sample(run_id: i64, ts: i64, bytes: i64) -> Sample {
    return Sample {
        ts,
        run_id,
        bytes_rx_wire: bytes,
        ..Sample::default()
    };
}

#[tokio::test]
async fn stats_reflect_counts_and_crawler_state() {
    let s = server().await;
    let (status, body) = get(&s, "/api/stats").await;
    assert_eq!(status, 200);
    assert_eq!(body["counts"]["companies"], 2);
    assert_eq!(body["counts"]["open_jobs"], 1);
    assert_eq!(body["crawler"]["running"], false);

    let run_id = events::append(&s.pool, &Event::CrawlerStarted { pid: 1 })
        .await
        .unwrap();
    samples::insert(&s.pool, &sample(run_id, now_ms() - 2000, 100))
        .await
        .unwrap();
    samples::insert(&s.pool, &sample(run_id, now_ms(), 1100))
        .await
        .unwrap();
    let (_, body) = get(&s, "/api/stats").await;
    assert_eq!(body["crawler"]["running"], true);
    assert_eq!(body["crawler"]["paused"], false);
    assert!(
        body["metrics"]["rates"]["bytes_rx_wire_per_sec"]
            .as_f64()
            .unwrap()
            > 0.0
    );

    let pause = Event::ControlApplied {
        command: "pause".into(),
        source: "ui".into(),
    };
    events::append(&s.pool, &pause).await.unwrap();
    assert_eq!(get(&s, "/api/stats").await.1["crawler"]["paused"], true);

    events::append(
        &s.pool,
        &Event::CrawlerStopped {
            reason: "done".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(get(&s, "/api/stats").await.1["crawler"]["running"], false);
}

#[tokio::test]
async fn graph_ranks_nodes_and_keeps_edges_between_them() {
    let s = server().await;
    let (_, body) = get(&s, "/api/graph").await;
    let hosts: Vec<&str> = body["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["host"].as_str().unwrap())
        .collect();
    assert_eq!(hosts, ["acme.com", "vc.com", "linked.com"]);
    assert_eq!(body["nodes"][0]["jobs"], 1, "closed jobs don't count");
    assert_eq!(body["edges"].as_array().unwrap().len(), 2);

    let (_, body) = get(&s, "/api/graph?discovered=false").await;
    assert_eq!(body["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(
        body["edges"].as_array().unwrap().len(),
        1,
        "edge to the dropped node goes too"
    );
    assert_eq!(body["total_domains"], 3);

    let (_, body) = get(&s, "/api/graph?limit=1").await;
    assert_eq!(body["nodes"].as_array().unwrap().len(), 1);
    assert!(body["edges"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn domain_detail_and_unknown_domain() {
    let s = server().await;
    let (status, body) = get(&s, "/api/domains/acme.com").await;
    assert_eq!(status, 200);
    assert_eq!(
        body["domain"]["score_reasons"]["signals"][0], "org_json_ld",
        "reasons are JSON, not a string"
    );
    assert_eq!(
        body["pages"][0]["kind"], "careers",
        "careers page listed first"
    );
    assert_eq!(
        (body["open_jobs"].clone(), body["inbound"].clone()),
        (1.into(), 1.into())
    );
    assert_eq!(body["page_count"], 2);
    assert_eq!(get(&s, "/api/domains/nope.com").await.0, 404);
}

#[tokio::test]
async fn events_latest_and_after() {
    let s = server().await;
    let mut ids = Vec::new();
    for pid in 1..=3 {
        ids.push(
            events::append(&s.pool, &Event::CrawlerStarted { pid })
                .await
                .unwrap(),
        );
    }
    let (_, latest) = get(&s, "/api/events?limit=2").await;
    let got: Vec<i64> = latest
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_i64().unwrap())
        .collect();
    assert_eq!(got, ids[1..], "latest two, oldest first");
    let (_, after) = get(&s, &format!("/api/events?after_id={}", ids[0])).await;
    assert_eq!(after.as_array().unwrap().len(), 2);
    assert_eq!(after[0]["event"]["kind"], "crawler_started");
}

#[tokio::test]
async fn control_commands_are_queued_for_the_crawler() {
    let s = server().await;
    let (status, body) = post(&s, "/api/control/pause").await;
    assert_eq!((status, body["command"].as_str()), (202, Some("pause")));
    assert_eq!(post(&s, "/api/control/explode").await.0, 400);
    let pending = career_core::control::pending(&s.pool).await.unwrap();
    assert_eq!(
        pending,
        [(
            body["id"].as_i64().unwrap(),
            Some(career_core::control::Command::Pause)
        )]
    );
}

#[tokio::test]
async fn metrics_history_is_downsampled() {
    let s = server().await;
    let now = now_ms();
    for i in 0..20 {
        samples::insert(&s.pool, &sample(1, now - 20_000 + i * 1000, i))
            .await
            .unwrap();
    }
    let (_, body) = get(
        &s,
        &format!(
            "/api/metrics/history?from={}&to={now}&step=5000",
            now - 20_000
        ),
    )
    .await;
    let n = body.as_array().unwrap().len();
    assert!((4..=5).contains(&n), "20 samples in 5s buckets, got {n}");
}

#[tokio::test]
async fn websocket_streams_new_events_and_samples() {
    let s = server().await;
    let ws_url = s.base.replace("http://", "ws://") + "/ws";
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
    // Let the tailer start after the seed data, then write new rows.
    tokio::time::sleep(Duration::from_millis(100)).await;
    events::append(&s.pool, &Event::CrawlerStarted { pid: 42 })
        .await
        .unwrap();
    samples::insert(&s.pool, &sample(1, now_ms(), 10))
        .await
        .unwrap();

    let mut kinds = Vec::new();
    while kinds.len() < 2 {
        let msg = tokio::time::timeout(Duration::from_secs(3), ws.next())
            .await
            .expect("live message")
            .unwrap()
            .unwrap();
        let data: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        kinds.push(data["type"].as_str().unwrap().to_string());
        if data["type"] == "event" {
            assert_eq!(data["event"]["pid"], 42);
        }
    }
    assert_eq!(kinds, ["event", "metrics"]);
}

async fn event_at(pool: &SqlitePool, ts: i64, event: &Event) {
    sqlx::query("INSERT INTO events (ts, kind, payload) VALUES (?, ?, ?)")
        .bind(ts)
        .bind(event.kind())
        .bind(serde_json::to_string(event).unwrap())
        .execute(pool)
        .await
        .unwrap();
}

fn classified(domain: &str, status: &str) -> Event {
    return Event::DomainClassified {
        domain: domain.into(),
        name: None,
        status: status.into(),
        previous: String::new(),
        score: 0.5,
    };
}

#[tokio::test]
async fn graph_replays_status_pages_and_edges_at_a_moment() {
    let s = server().await;
    sqlx::query("UPDATE domains SET first_seen = 50 WHERE host = 'linked.com'")
        .execute(&s.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE edges SET first_seen = 50 WHERE dst_domain_id = 3")
        .execute(&s.pool)
        .await
        .unwrap();
    event_at(&s.pool, 1, &classified("acme.com", "probing")).await;
    event_at(&s.pool, 2, &classified("acme.com", "company")).await;
    event_at(&s.pool, 3, &classified("vc.com", "company")).await;

    let node = |g: &Value, host: &str| -> Value {
        return g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["host"] == host)
            .cloned()
            .unwrap_or(Value::Null);
    };
    let (_, early) = get(&s, "/api/graph?at=1").await;
    assert_eq!(node(&early, "acme.com")["status"], "probing");
    assert_eq!(
        node(&early, "acme.com")["pages"],
        1,
        "only the page fetched at t=1"
    );
    assert_eq!(
        node(&early, "vc.com")["status"],
        "discovered",
        "not classified yet at t=1"
    );
    assert_eq!(
        node(&early, "linked.com"),
        Value::Null,
        "first seen at t=50"
    );
    assert_eq!(early["edges"].as_array().unwrap().len(), 1);
    assert_eq!(early["counts"]["companies"], 0);
    assert_eq!(early["at"], 1);

    let (_, later) = get(&s, "/api/graph?at=10").await;
    assert_eq!(node(&later, "acme.com")["status"], "company");
    assert_eq!(later["counts"]["companies"], 2);
    assert_eq!(later["counts"]["pages"], 3);

    let (_, live) = get(&s, "/api/graph").await;
    assert_eq!(live["counts"]["domains"], 3);
    assert_eq!(live["edges"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn history_lists_runs_and_activity() {
    let s = server().await;
    let fetched = |url: &str| Event::PageFetched {
        url: url.into(),
        domain: "acme.com".into(),
        status: 200,
        depth: 0,
        links: 0,
        enqueued: 0,
        duplicate: false,
        bytes_wire: 0,
    };
    event_at(&s.pool, 1_000, &Event::CrawlerStarted { pid: 1 }).await;
    event_at(&s.pool, 2_000, &fetched("https://acme.com/")).await;
    event_at(
        &s.pool,
        3_000,
        &Event::CrawlerStopped {
            reason: "max_pages".into(),
        },
    )
    .await;
    event_at(&s.pool, 10_000, &Event::CrawlerStarted { pid: 2 }).await;
    event_at(&s.pool, 11_000, &fetched("https://acme.com/careers")).await;

    let (status, h) = get(&s, "/api/history?buckets=10").await;
    assert_eq!(status, 200);
    assert_eq!(h["start"], 1_000);
    let runs = h["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(
        (runs[0]["stopped_at"].clone(), runs[0]["reason"].clone()),
        (3_000.into(), "max_pages".into())
    );
    assert_eq!(runs[1]["stopped_at"], Value::Null, "still running");
    let pages: i64 = h["activity"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["pages"].as_i64().unwrap())
        .sum();
    assert_eq!(pages, 2);

    let (_, feed) = get(&s, "/api/events?before=2500&limit=10").await;
    let kinds: Vec<&str> = feed
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["crawler_started", "page_fetched"]);
}

#[tokio::test]
async fn domain_page_graph_links_pages_pending_and_external_sites() {
    let s = server().await;
    let acme_home: i64 = sqlx::query_scalar("SELECT id FROM pages WHERE url = 'https://acme.com/'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    for dst in [
        "https://acme.com/careers",
        "https://acme.com/about",
        "https://blog.acme.com/post",
        "https://vc.com/portfolio",
        "https://vc.com/team",
    ] {
        sqlx::query("INSERT INTO page_links (src_page_id, dst_url) VALUES (?, ?)")
            .bind(acme_home)
            .bind(dst)
            .execute(&s.pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO frontier (url, score, depth, state, enqueued_at) VALUES ('https://acme.com/about', 1, 1, 'deferred', 0)")
        .execute(&s.pool)
        .await
        .unwrap();

    let (status, g) = get(&s, "/api/domains/acme.com/graph").await;
    assert_eq!(status, 200);
    let find = |id: &str| {
        g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(find(&format!("p:{acme_home}"))["label"], "acme.com");
    assert_eq!(find("u:https://acme.com/about")["state"], "deferred");
    assert_eq!(
        find("u:https://blog.acme.com/post")["kind"],
        "pending",
        "subdomains are internal"
    );
    assert_eq!(
        find("d:vc.com")["links"],
        2,
        "external links collapse per site"
    );
    let edges = g["edges"].as_array().unwrap();
    assert_eq!(
        edges.len(),
        4,
        "careers page, two pending pages, one external site"
    );
    assert!(
        edges
            .iter()
            .any(|e| e["target"] == "d:vc.com" && e["weight"] == 2)
    );
    assert_eq!(get(&s, "/api/domains/nope.com/graph").await.0, 404);
}

#[tokio::test]
async fn frontend_is_served() {
    let s = server().await;
    for (path, title) in [
        ("/", "Dashboard"),
        ("/graph", "Graph"),
        ("/companies", "Companies"),
        ("/search", "Job search"),
        ("/profile", "Profile"),
        ("/resources", "Resources"),
    ] {
        let resp = reqwest::get(format!("{}{path}", s.base)).await.unwrap();
        assert_eq!(resp.status(), 200, "{path}");
        let html = resp.text().await.unwrap();
        assert!(
            html.contains(&format!("<title>{title} · Career Crawler</title>")),
            "{path}"
        );
    }
    for (file, content_type) in [
        ("common.js", "text/javascript"),
        ("shell.js", "text/javascript"),
        ("feed.js", "text/javascript"),
        ("metrics.js", "text/javascript"),
        ("page-dashboard.js", "text/javascript"),
        ("page-graph.js", "text/javascript"),
        ("page-companies.js", "text/javascript"),
        ("page-search.js", "text/javascript"),
        ("page-profile.js", "text/javascript"),
        ("page-resources.js", "text/javascript"),
        ("graph.js", "text/javascript"),
        ("charts.js", "text/javascript"),
        ("profile.js", "text/javascript"),
        ("search.js", "text/javascript"),
        ("style.css", "text/css"),
    ] {
        let resp = reqwest::get(format!("{}/static/{file}", s.base))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{file}");
        let got = resp.headers()["content-type"].to_str().unwrap().to_string();
        assert!(got.starts_with(content_type), "{file}: {got}");
    }
    assert_eq!(
        reqwest::get(format!("{}/static/secret.txt", s.base))
            .await
            .unwrap()
            .status(),
        404
    );
}

const CV: &str = "Jane Doe\nSenior Backend Engineer\nLagos, Nigeria\n\n## Experience\nSenior Backend Engineer, Acme | 2019 - Present\n- Rust and PostgreSQL on AWS.\n\n## Skills\nRust, SQL, AWS";

async fn upload(s: &Server, filename: &str, body: Vec<u8>) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{}/api/profile/cv?filename={filename}", s.base))
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    return (
        status,
        serde_json::from_str(&resp.text().await.unwrap()).unwrap_or(Value::Null),
    );
}

async fn put(s: &Server, path: &str, body: Value) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .put(format!("{}{path}", s.base))
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    return (
        status,
        serde_json::from_str(&resp.text().await.unwrap()).unwrap_or(Value::Null),
    );
}

/// The recompute after a change runs in the background.
async fn until_matched(s: &Server) -> Value {
    for _ in 0..100 {
        let (_, body) = get(s, "/api/profile/matches").await;
        if body["profile"] == true && body["stale"] == false {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("matches never caught up");
}

#[tokio::test]
async fn no_profile_until_a_cv_is_uploaded() {
    let s = server().await;
    let (status, body) = get(&s, "/api/profile").await;
    assert_eq!(status, 200);
    assert!(body["profile"].is_null());
    let (_, matches) = get(&s, "/api/profile/matches").await;
    assert_eq!(matches["matches"].as_array().unwrap().len(), 0);
    let (status, _) = put(
        &s,
        "/api/profile/overrides",
        serde_json::json!({"relocate": true}),
    )
    .await;
    assert_eq!(status, 409, "nothing to edit yet");
}

#[tokio::test]
async fn uploading_a_cv_creates_the_profile_and_ranks_jobs() {
    let s = server().await;
    let (status, body) = upload(&s, "jane.md", CV.as_bytes().to_vec()).await;
    assert_eq!(status, 200, "{body}");
    let p = &body["profile"];
    assert_eq!(p["source"], "parser");
    assert_eq!(p["name"], "jane");
    assert!(
        p["merged"]["skills"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "rust")
    );
    assert_eq!(p["merged"]["locations"][0]["name"], "Lagos, NG");

    let matches = until_matched(&s).await;
    // The seeded open job ("Engineer") is ranked; the closed one is not.
    let rows = matches["matches"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["title"], "Engineer");
    assert!(rows[0]["score"].as_f64().unwrap() > 0.0);
    assert_eq!(rows[0]["company"], "Acme");
    assert_eq!(matches["total"], 1);

    // Filters narrow the list and paging leaves the total alone.
    let (_, none) = get(&s, "/api/profile/matches?q=zzz").await;
    assert_eq!(
        (
            none["total"].as_i64(),
            none["matches"].as_array().unwrap().len()
        ),
        (Some(0), 0)
    );
    let (_, page2) = get(&s, "/api/profile/matches?offset=1").await;
    assert_eq!(
        (
            page2["total"].as_i64(),
            page2["matches"].as_array().unwrap().len()
        ),
        (Some(1), 0)
    );
    let (_, hit) = get(&s, "/api/profile/matches?q=acme&sort=recent").await;
    assert_eq!(hit["total"], 1);
}

#[tokio::test]
async fn bad_uploads_are_refused_with_a_reason() {
    let s = server().await;
    for (name, body, needle) in [
        (
            "cv.docx",
            b"whatever whatever whatever whatever whatever".to_vec(),
            "unsupported",
        ),
        ("cv.pdf", CV.as_bytes().to_vec(), "not a PDF"),
        ("cv.txt", vec![0xff, 0xfe, 0x00], "UTF-8"),
        ("cv.txt", Vec::new(), "empty"),
        ("cv.txt", b"hi".to_vec(), "no extractable text"),
        ("", CV.as_bytes().to_vec(), "unsupported"),
    ] {
        let (status, err) = upload(&s, name, body).await;
        assert_eq!(status, 400, "{name}");
        assert!(
            err["error"].as_str().unwrap().contains(needle),
            "{name}: {err}"
        );
    }
    assert!(get(&s, "/api/profile").await.1["profile"].is_null());
    let profiles: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM profiles")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(profiles, 0);
}

#[tokio::test]
async fn edits_win_over_the_cv_and_survive_a_re_upload() {
    let s = server().await;
    upload(&s, "jane.md", CV.as_bytes().to_vec()).await;
    let (status, body) = put(
        &s,
        "/api/profile/overrides",
        serde_json::json!({
            "industries": ["Fintech", " fintech ", ""],
            "locations": ["Accra, Ghana"],
            "skills": ["Go", {"name": "SQL", "weight": 9}],
            "seniority": "Lead",
            "exclude": ["crypto"],
            "relocate": true,
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let merged = &body["profile"]["merged"];
    assert_eq!(merged["industries"], serde_json::json!(["Fintech"]));
    assert_eq!(merged["locations"][0]["name"], "Accra, GH");
    assert_eq!(merged["seniority"], "lead");
    assert_eq!(merged["skills"][1]["weight"], 1.0, "weights are clamped");
    assert_eq!(merged["exclude"], serde_json::json!(["crypto"]));
    assert_eq!(
        body["profile"]["extracted"]["locations"][0]["name"], "Lagos, NG",
        "the CV's reading is untouched"
    );

    // Clearing a key goes back to what the CV said.
    let (_, body) = put(
        &s,
        "/api/profile/overrides",
        serde_json::json!({"locations": null, "relocate": null}),
    )
    .await;
    assert_eq!(
        body["profile"]["merged"]["locations"][0]["name"],
        "Lagos, NG"
    );
    assert_eq!(
        body["profile"]["merged"]["seniority"], "lead",
        "other edits stay"
    );

    // Uploading the same CV again brings the same profile back with its edits.
    let (_, again) = upload(&s, "renamed.md", CV.as_bytes().to_vec()).await;
    assert_eq!(again["profile"]["id"], body["profile"]["id"]);
    assert_eq!(again["profile"]["merged"]["seniority"], "lead");
}

#[tokio::test]
async fn edit_validation() {
    let s = server().await;
    upload(&s, "jane.md", CV.as_bytes().to_vec()).await;
    for patch in [
        serde_json::json!({"seniority": "wizard"}),
        serde_json::json!({"remote": "sometimes"}),
        serde_json::json!({"titles": "not a list"}),
        serde_json::json!({"salary_expectation_usd": -5}),
        serde_json::json!({"favourite_colour": "green"}),
        serde_json::json!({"favourite_colour": null}),
    ] {
        let (status, body) = put(&s, "/api/profile/overrides", patch.clone()).await;
        assert_eq!(status, 400, "{patch}: {body}");
    }
}

#[tokio::test]
async fn removing_the_profile_deactivates_it() {
    let s = server().await;
    upload(&s, "jane.md", CV.as_bytes().to_vec()).await;
    let resp = reqwest::Client::new()
        .delete(format!("{}/api/profile", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(get(&s, "/api/profile").await.1["profile"].is_null());
}

async fn send(
    s: &Server,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut req = reqwest::Client::new().request(method, format!("{}{path}", s.base));
    if let Some(b) = body {
        req = req
            .header("content-type", "application/json")
            .body(b.to_string());
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    return (
        status,
        serde_json::from_str(&resp.text().await.unwrap()).unwrap_or(Value::Null),
    );
}

#[tokio::test]
async fn profiles_can_be_listed_switched_renamed_edited_and_deleted() {
    use reqwest::Method;
    let s = server().await;
    let (_, empty) = get(&s, "/api/profiles").await;
    assert_eq!(empty["profiles"].as_array().unwrap().len(), 0);

    let (_, first) = upload(&s, "jane.md", CV.as_bytes().to_vec()).await;
    let a = first["profile"]["id"].as_i64().unwrap();
    let other = b"Someone Else\nData Analyst with several years of SQL and Excel work";
    let (_, second) = upload(&s, "other.txt", other.to_vec()).await;
    let b = second["profile"]["id"].as_i64().unwrap();

    // The newest upload is active and comes first.
    let (_, list) = get(&s, "/api/profiles").await;
    let rows = list["profiles"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (rows[0]["id"].as_i64(), rows[0]["active"].clone()),
        (Some(b), true.into())
    );
    assert_eq!(rows[1]["active"], false);

    // Switching makes the other one active, and only one.
    let (status, body) = send(
        &s,
        Method::POST,
        &format!("/api/profiles/{a}/activate"),
        None,
    )
    .await;
    assert_eq!(
        (status, body["profile"]["active"].clone()),
        (200, true.into())
    );
    let (_, now) = get(&s, "/api/profile").await;
    assert_eq!(now["profile"]["id"].as_i64(), Some(a));
    let (_, list) = get(&s, "/api/profiles").await;
    assert_eq!(
        list["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["active"] == true)
            .count(),
        1
    );

    // Edits and renames reach an inactive profile without activating it.
    let (status, body) = send(
        &s,
        Method::PUT,
        &format!("/api/profiles/{b}/overrides"),
        Some(serde_json::json!({"relocate": true})),
    )
    .await;
    assert_eq!(
        (
            status,
            body["profile"]["merged"]["relocate"].clone(),
            body["profile"]["active"].clone()
        ),
        (200, true.into(), false.into())
    );
    let (status, body) = send(
        &s,
        Method::PATCH,
        &format!("/api/profiles/{b}"),
        Some(serde_json::json!({"name": "  Analyst  "})),
    )
    .await;
    assert_eq!(
        (status, body["profile"]["name"].clone()),
        (200, "Analyst".into())
    );
    let (status, _) = send(
        &s,
        Method::PATCH,
        &format!("/api/profiles/{b}"),
        Some(serde_json::json!({"name": "  "})),
    )
    .await;
    assert_eq!(status, 400);
    let (_, list) = get(&s, "/api/profiles").await;
    let edited = list["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"].as_i64() == Some(b))
        .unwrap()
        .clone();
    assert_eq!(edited["edited_fields"], 1);

    // Deactivating keeps it stored; deleting the active one leaves none active.
    send(
        &s,
        Method::POST,
        &format!("/api/profiles/{a}/deactivate"),
        None,
    )
    .await;
    let (_, none) = get(&s, "/api/profile").await;
    assert!(none["profile"].is_null());
    send(
        &s,
        Method::POST,
        &format!("/api/profiles/{a}/activate"),
        None,
    )
    .await;
    let (status, _) = send(&s, Method::DELETE, &format!("/api/profiles/{a}"), None).await;
    assert_eq!(status, 200);
    let (_, none) = get(&s, "/api/profile").await;
    assert!(none["profile"].is_null());
    let (_, list) = get(&s, "/api/profiles").await;
    assert_eq!(list["profiles"].as_array().unwrap().len(), 1);

    // Unknown ids are 404s.
    for (method, path) in [
        (Method::GET, format!("/api/profiles/{a}")),
        (Method::DELETE, format!("/api/profiles/{a}")),
        (Method::POST, format!("/api/profiles/{a}/activate")),
        (Method::GET, format!("/api/profiles/{a}/matches")),
    ] {
        assert_eq!(send(&s, method, &path, None).await.0, 404, "{path}");
    }
}

#[tokio::test]
async fn an_oversized_upload_is_rejected() {
    let s = server().await;
    let (status, _) = upload(
        &s,
        "big.txt",
        vec![b'a'; career_cv::text::MAX_BYTES + 10_000],
    )
    .await;
    assert_eq!(status, 413, "the body limit stops it before it is read");
}

// ---------- natural-language search ----------

/// A stored job with the fields search reads, as the enricher would have left them.
async fn search_job(
    pool: &SqlitePool,
    url: &str,
    title: &str,
    salary_usd: Option<f64>,
    skill: &str,
) {
    sqlx::query(
        "INSERT INTO jobs (url, domain_id, title, description, skills, category, lat, lon, country_code,
                           salary_usd_annual, source, first_seen, last_seen)
         VALUES (?, 1, ?, ?, ?, 'engineering', 6.5244, 3.3792, 'NG', ?, 'jsonld', 0, 0)",
    )
    .bind(url)
    .bind(title)
    .bind(format!("You will write {skill} all day."))
    .bind(serde_json::json!([skill]).to_string())
    .bind(salary_usd)
    .execute(pool)
    .await
    .unwrap();
}

async fn post_json(s: &Server, path: &str, body: Value) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{}{path}", s.base))
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    return (
        status,
        serde_json::from_str(&resp.text().await.unwrap()).unwrap_or(Value::Null),
    );
}

#[tokio::test]
async fn nl_search_applies_the_filters_it_is_given() {
    let s = server().await;
    search_job(
        &s.pool,
        "https://acme.com/paid",
        "Paid Engineer",
        Some(120_000.0),
        "rust",
    )
    .await;
    search_job(
        &s.pool,
        "https://acme.com/unpaid",
        "Unpaid Engineer",
        None,
        "python",
    )
    .await;

    let (status, body) = post_json(
        &s,
        "/api/search/nl",
        serde_json::json!({"filters": {
            "keywords": ["engineer"],
            "salary": {"mode": "min_usd", "value": 100_000},
        }}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["source"], "filters");
    assert_eq!(
        body["sort"], "relevance",
        "the words rank when there is no CV"
    );
    let titles: Vec<&str> = body["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Paid Engineer"], "the seed job posts no salary");
    assert_eq!(body["hits"][0]["skills"][0], "rust");
    assert!(
        body["notes"][0]
            .as_str()
            .unwrap()
            .contains("at least $100000/yr"),
        "{}",
        body["notes"]
    );
}

#[tokio::test]
async fn nl_search_without_an_llm_reads_the_words_as_keywords() {
    let s = server().await;
    search_job(
        &s.pool,
        "https://acme.com/paid",
        "Paid Engineer",
        Some(120_000.0),
        "rust",
    )
    .await;

    let (status, body) = post_json(
        &s,
        "/api/search/nl",
        serde_json::json!({"query": "I am looking for nice paying engineer jobs in Lagos"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["source"], "keywords");
    assert_eq!(
        body["query"]["keywords"],
        serde_json::json!(["engineer", "lagos"])
    );
    assert!(
        body["notes"][0].as_str().unwrap().contains("no LLM"),
        "{}",
        body["notes"]
    );
    let titles: Vec<&str> = body["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        ["Paid Engineer", "Engineer"],
        "every open job mentioning engineer; \"lagos\" matches no text"
    );
}

#[tokio::test]
async fn nl_search_reads_the_words_with_the_llm_and_caches_the_reading() {
    let provider = Arc::new(FakeProvider::replying(
        r#"{"keywords": ["rust"], "categories": ["engineering"], "explanation": "Rust engineering roles"}"#,
    ));
    let s = server_with(|state| {
        state.llm = Some(Arc::new(Llm::new(
            &LlmConfig::default(),
            state.pool.clone(),
            provider.clone(),
        )));
        state.llm_host = Some("http://fake.invalid".into());
    })
    .await;
    search_job(
        &s.pool,
        "https://acme.com/rust",
        "Rust Engineer",
        Some(120_000.0),
        "rust",
    )
    .await;

    let payload = serde_json::json!({"query": "roles where I can write Rust"});
    let (status, body) = post_json(&s, "/api/search/nl", payload.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["source"], "llm");
    assert_eq!(body["query"]["explanation"], "Rust engineering roles");
    assert_eq!(
        body["query"]["categories"],
        serde_json::json!(["engineering"])
    );
    let titles: Vec<&str> = body["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Rust Engineer"]);
    assert_eq!(provider.call_count(), 1);
    let sent = provider.requests()[0].messages[1].content.clone();
    assert!(
        sent.contains("query: roles where I can write Rust"),
        "{sent}"
    );

    // The same words don't pay twice: the reading is cached.
    let (_, again) = post_json(&s, "/api/search/nl", payload).await;
    assert_eq!(again["hits"][0]["title"], "Rust Engineer");
    assert_eq!(
        provider.call_count(),
        1,
        "the second search was served from the cache"
    );
}

#[tokio::test]
async fn nl_search_rejects_nonsense_instead_of_guessing() {
    let s = server().await;
    assert_eq!(
        post_json(&s, "/api/search/nl", serde_json::json!({}))
            .await
            .0,
        400
    );
    assert_eq!(
        post_json(&s, "/api/search/nl", serde_json::json!({"query": "  "}))
            .await
            .0,
        400
    );
    // A filter the app doesn't have is dropped and explained, not fatal.
    let (status, body) = post_json(
        &s,
        "/api/search/nl",
        serde_json::json!({"filters": {"categories": ["wizard"], "limit": 9_999}}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["query"]["categories"], serde_json::json!([]));
    assert_eq!(body["query"]["limit"], 200);
    assert!(
        body["notes"][0].as_str().unwrap().contains("wizard"),
        "{}",
        body["notes"]
    );
}

#[tokio::test]
async fn nl_search_ranks_by_the_profile_when_there_is_one() {
    let s = server().await;
    upload(&s, "jane.md", CV.as_bytes().to_vec()).await;
    until_matched(&s).await;

    let (status, body) = post_json(
        &s,
        "/api/search/nl",
        serde_json::json!({"filters": {"keywords": ["engineer"]}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["sort"], "match", "the CV profile decides the order");
    assert!(body["hits"][0]["match_score"].as_f64().unwrap() > 0.0);
}

#[tokio::test]
async fn places_can_be_searched_and_only_known_places_can_be_added() {
    let s = server().await;
    let (status, found) = get(&s, "/api/places?q=lag").await;
    assert_eq!(status, 200);
    assert_eq!(found[0]["value"], "Lagos, Nigeria");
    assert_eq!(get(&s, "/api/places?q=").await.1, serde_json::json!([]));

    upload(&s, "jane.md", CV.as_bytes().to_vec()).await;
    // Picking a suggestion works, several at once, and the whole country too.
    let (status, body) = put(
        &s,
        "/api/profile/overrides",
        serde_json::json!({"locations": ["Accra, Ghana", "Nigeria"]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let places = body["profile"]["merged"]["locations"].as_array().unwrap();
    assert_eq!(places[0]["name"], "Accra, GH");
    assert!(places[0]["lat"].is_number());
    assert_eq!(places[1]["name"], "NG");

    let (status, err) = put(
        &s,
        "/api/profile/overrides",
        serde_json::json!({"locations": ["Atlantis"]}),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        err["error"]
            .as_str()
            .unwrap()
            .contains("pick one of the suggestions"),
        "{err}"
    );
    let (status, _) = put(
        &s,
        "/api/profile/overrides",
        serde_json::json!({"locations": [null]}),
    )
    .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn companies_filter_sort_and_page() {
    let s = server().await;
    let (code, all) = get(&s, "/api/companies?status=all&sort=host&desc=false").await;
    assert_eq!(code, 200);
    assert_eq!(all["total"], 3);
    assert_eq!(all["rows"][0]["host"], "acme.com");
    assert_eq!(all["rows"][0]["open_jobs"], 1);
    assert_eq!(all["rows"][0]["pages"], 2);

    let (_, co) = get(&s, "/api/companies?status=company").await;
    assert_eq!(co["total"], 2);
    let (_, jobs) = get(&s, "/api/companies?status=all&has_jobs=true").await;
    assert_eq!(jobs["total"], 1);
    let (_, careers) = get(&s, "/api/companies?status=all&has_careers=false").await;
    assert_eq!(careers["total"], 2);
    let (_, q) = get(&s, "/api/companies?status=all&q=vc").await;
    assert_eq!(q["rows"][0]["host"], "vc.com");
    // LIKE wildcards in the search text are literal.
    let (_, wild) = get(&s, "/api/companies?status=all&q=%25").await;
    assert_eq!(wild["total"], 0);
    let (_, paged) = get(
        &s,
        "/api/companies?status=all&sort=host&desc=false&limit=1&offset=1",
    )
    .await;
    assert_eq!(paged["total"], 3);
    assert_eq!(paged["rows"][0]["host"], "linked.com");
}
