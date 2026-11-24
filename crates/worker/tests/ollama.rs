use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use serde_json::{Value, json};
use shared::TranslationRequest;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use worker::{Ollama, TranslateError};

struct Mock {
    replies: VecDeque<(StatusCode, Value)>,
    calls: Vec<Value>,
    delay: Duration,
}
type MockState = Arc<Mutex<Mock>>;
async fn chat(
    State(state): State<MockState>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let (reply, delay) = {
        let mut state = state.lock().unwrap();
        state.calls.push(body);
        (
            state
                .replies
                .pop_front()
                .unwrap_or((StatusCode::INTERNAL_SERVER_ERROR, json!({}))),
            state.delay,
        )
    };
    tokio::time::sleep(delay).await;
    (reply.0, Json(reply.1))
}
fn ok(content: &str) -> (StatusCode, Value) {
    (
        StatusCode::OK,
        json!({"done":true,"done_reason":"stop","message":{"content":content}}),
    )
}
fn request() -> TranslationRequest {
    serde_json::from_str(include_str!("../../../tests/fixtures/en.json")).unwrap()
}
async fn mock(
    replies: Vec<(StatusCode, Value)>,
    delay: Duration,
    attempts: u32,
) -> (Ollama, MockState, tokio::task::JoinHandle<()>) {
    let state = Arc::new(Mutex::new(Mock {
        replies: replies.into(),
        calls: vec![],
        delay,
    }));
    let app = Router::new()
        .route("/api/chat", post(chat))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (
        Ollama::new(&url, "test-model".into(), 1, attempts).unwrap(),
        state,
        task,
    )
}
const VALID: &str =
    r#"{"translations":["晨光落在窗前","我帶著昨日走入雨中","","今晚我們的腳步再度找到歸路"]}"#;

#[tokio::test]
async fn sends_full_context_and_preserves_order_and_blank_lines() {
    let (client, state, server) = mock(vec![ok(VALID)], Duration::ZERO, 1).await;
    let result = client.translate(&request()).await.unwrap();
    assert_eq!(result.translations[2], "");
    assert_eq!(result.translations[0], "晨光落在窗前");
    let state = state.lock().unwrap();
    let body = &state.calls[0];
    assert_eq!(body["stream"], false);
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["format"]["properties"]["translations"]["minItems"], 4);
    let prompt = body["messages"][0]["content"].as_str().unwrap();
    for line in request().lyrics {
        assert!(prompt.contains(&line));
    }
    server.abort();
}

#[tokio::test]
async fn retries_bad_json_wrong_count_and_server_errors() {
    for reply in [
        ok("not json"),
        ok(r#"{"translations":["一行"]}"#),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":"private data"}),
        ),
        ok(r#"{"translations":["一","二","非空白","四"]}"#),
    ] {
        let (client, state, server) = mock(vec![reply, ok(VALID)], Duration::ZERO, 2).await;
        assert!(client.translate(&request()).await.is_ok());
        assert_eq!(state.lock().unwrap().calls.len(), 2);
        server.abort();
    }
}

#[tokio::test]
async fn retries_are_bounded_and_http_errors_are_sanitized() {
    let (client, state, server) = mock(vec![ok("bad"), ok("bad")], Duration::ZERO, 2).await;
    assert_eq!(
        client.translate(&request()).await.unwrap_err(),
        TranslateError::InvalidTranslation
    );
    assert_eq!(state.lock().unwrap().calls.len(), 2);
    server.abort();
    let (client, state, server) = mock(
        vec![(StatusCode::NOT_FOUND, json!({"error":"secret lyrics"}))],
        Duration::ZERO,
        3,
    )
    .await;
    assert_eq!(
        client.translate(&request()).await.unwrap_err().to_string(),
        "Http(404)"
    );
    assert_eq!(state.lock().unwrap().calls.len(), 1);
    server.abort();
}

#[tokio::test]
async fn invalid_input_never_calls_model_and_timeout_is_bounded() {
    let (client, state, server) = mock(vec![ok(VALID)], Duration::from_secs(2), 1).await;
    let mut invalid = request();
    invalid.source_language = "fr".into();
    assert_eq!(
        client.translate(&invalid).await.unwrap_err(),
        TranslateError::InvalidInput
    );
    assert!(state.lock().unwrap().calls.is_empty());
    assert_eq!(
        client.translate(&request()).await.unwrap_err(),
        TranslateError::Transport
    );
    server.abort();
}

#[tokio::test]
async fn rejects_truncated_and_multiline_outputs() {
    for reply in [
        (
            StatusCode::OK,
            json!({"done":true,"done_reason":"length","message":{"content":VALID}}),
        ),
        ok(r#"{"translations":["一\n二","三","","四"]}"#),
    ] {
        let (client, _, server) = mock(vec![reply], Duration::ZERO, 1).await;
        assert!(client.translate(&request()).await.is_err());
        server.abort();
    }
}

#[test]
fn rejects_invalid_configuration() {
    for (url, timeout, attempts) in [
        ("file:///tmp", 1, 1),
        ("http://user:secret@localhost", 1, 1),
        ("http://localhost", 0, 1),
        ("http://localhost", 1, 0),
        ("http://localhost", 1, 6),
    ] {
        assert!(Ollama::new(url, "test".into(), timeout, attempts).is_err());
    }
}

#[tokio::test]
#[ignore = "requires running local Ollama and OLLAMA_MODEL; performs real inference"]
async fn real_translategemma_all_languages() {
    let client = Ollama::from_env().unwrap();
    for fixture in [
        include_str!("../../../tests/fixtures/en.json"),
        include_str!("../../../tests/fixtures/ja.json"),
        include_str!("../../../tests/fixtures/ko.json"),
    ] {
        let request: TranslationRequest = serde_json::from_str(fixture).unwrap();
        let result = client.translate(&request).await.unwrap();
        assert_eq!(result.translations.len(), request.lyrics.len());
        assert_eq!(result.translations[2], "");
    }
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; creates and drops an isolated schema"]
async fn queued_worker_recovers_failure_and_interruption() {
    use shared::queue::Queue;
    use sqlx::{PgPool, postgres::PgPoolOptions};
    use uuid::Uuid;
    let url = std::env::var("TEST_DATABASE_URL").expect("set TEST_DATABASE_URL");
    let admin = PgPool::connect(&url).await.unwrap();
    let schema = format!("worker_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let search_path = format!("SET search_path TO {schema}");
    let pool = PgPoolOptions::new()
        .after_connect(move |conn, _| {
            let query = search_path.clone();
            Box::pin(async move {
                sqlx::query(&query).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    let queue = Queue::new(pool.clone());
    queue.migrate().await.unwrap();
    let user = Uuid::new_v4();
    let job = queue.enqueue(user, &request(), "test-model").await.unwrap();
    let (client, _, server) = mock(
        vec![(StatusCode::SERVICE_UNAVAILABLE, json!({})), ok(VALID)],
        Duration::ZERO,
        1,
    )
    .await;
    assert!(worker::runner::process_one(&queue, &client).await.unwrap());
    assert_eq!(
        queue.get(user, job.job_id).await.unwrap().unwrap().status,
        "pending"
    );
    assert!(worker::runner::process_one(&queue, &client).await.unwrap());
    assert_eq!(
        queue.get(user, job.job_id).await.unwrap().unwrap().status,
        "completed"
    );
    assert!(!worker::runner::process_one(&queue, &client).await.unwrap());
    server.abort();

    let mut next = request();
    next.lyrics[0] = "An interrupted song".into();
    let job = queue.enqueue(user, &next, "test-model").await.unwrap();
    let (client, state, server) = mock(vec![ok(VALID)], Duration::from_secs(10), 1).await;
    let q = queue.clone();
    let task = tokio::spawn(async move { worker::runner::process_one(&q, &client).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !state.lock().unwrap().calls.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(
        queue.get(user, job.job_id).await.unwrap().unwrap().status,
        "processing"
    );
    assert!(queue.claim("test-model").await.unwrap().is_none());
    sqlx::query(
        "UPDATE translation_jobs SET lease_expires_at=now()-interval '1 second' WHERE id=$1",
    )
    .bind(job.job_id)
    .execute(&pool)
    .await
    .unwrap();
    server.abort();
    let (client, _, server) = mock(vec![ok(VALID)], Duration::ZERO, 1).await;
    assert!(worker::runner::process_one(&queue, &client).await.unwrap());
    assert_eq!(
        queue.get(user, job.job_id).await.unwrap().unwrap().status,
        "completed"
    );
    server.abort();
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
}
