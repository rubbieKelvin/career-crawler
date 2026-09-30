//! End-to-end tests: a real server on a random port over a seeded temp database.

use std::time::Duration;

use career_core::db;
use career_core::events::{self, Event};
use career_core::samples::{self, Sample};
use career_core::time::now_ms;
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
    let dir = tempfile::tempdir().unwrap();
    let pool = db::open(&dir.path().join("t.db")).await.unwrap();
    seed(&pool).await;
    let app = router(start(pool.clone(), Duration::from_millis(30)));
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

#[tokio::test]
async fn index_page_is_served() {
    let s = server().await;
    let html = reqwest::get(format!("{}/", s.base))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("<title>Career Crawler</title>"));
}
