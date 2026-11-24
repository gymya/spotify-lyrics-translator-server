use super::*;
use crate::test_support::*;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use jsonwebtoken::Algorithm;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

fn lazy_queue() -> Queue {
    Queue::new(
        PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(100))
            .connect_lazy("postgres://invalid:invalid@127.0.0.1:1/invalid")
            .unwrap(),
    )
}
fn state(server: &JwksServer, queue: Queue) -> AppState {
    AppState {
        queue,
        auth: Auth::new(&server.url).unwrap(),
        model: "api-test-model".into(),
    }
}
async fn call(
    app: Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    if response.status() == StatusCode::UNAUTHORIZED {
        assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");
    }
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 100000).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn public_health_authentication_errors_and_fallbacks() {
    let server = server().await;
    let app = app(state(&server, lazy_queue()));
    assert_eq!(
        call(app.clone(), "GET", "/health", None, "").await,
        (StatusCode::OK, json!({"status":"ok"}))
    );
    for path in ["/api/translations", "/api/translations/invalid"] {
        let method = if path == "/api/translations" {
            "POST"
        } else {
            "GET"
        };
        let (status, body) = call(app.clone(), method, path, None, "").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
        assert_eq!(
            call(app.clone(), method, path, Some("invalid"), "").await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        call(app.clone(), "GET", "/unknown", None, "").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(app, "POST", "/health", None, "").await.0,
        StatusCode::METHOD_NOT_ALLOWED
    );
}
#[tokio::test]
async fn validates_input_before_database_and_sanitizes_failures() {
    let server = server().await;
    let app = app(state(&server, lazy_queue()));
    let t = token(
        &claims(&server.url, Uuid::new_v4()),
        Algorithm::ES256,
        Some("ec-test"),
    );
    for body in [
        "{broken",
        r#"{"lyrics":["private"],"sourceLanguage":"fr","targetLanguage":"zh-TW"}"#,
        r#"{"lyrics":["a"],"sourceLanguage":"en","targetLanguage":"zh-TW","userId":"attacker"}"#,
    ] {
        let (status, body) = call(app.clone(), "POST", "/api/translations", Some(&t), body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_request");
        assert!(!body.to_string().contains("private"));
    }
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/translations",
            Some(&t),
            &"x".repeat(65537)
        )
        .await
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/translations/not-a-uuid",
            Some(&t),
            ""
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (status, body) = call(
        app,
        "POST",
        "/api/translations",
        Some(&t),
        include_str!("../../../tests/fixtures/en.json"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "service_unavailable");
    assert!(!body.to_string().contains("postgres"));
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; isolated PostgreSQL schema and signed mock JWTs"]
async fn authenticated_api_queue_ownership_and_concurrency() {
    let url = std::env::var("TEST_DATABASE_URL").expect("set TEST_DATABASE_URL");
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    let schema = format!("api_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let path = format!("SET search_path TO {schema}");
    let pool = PgPoolOptions::new()
        .max_connections(12)
        .after_connect(move |conn, _| {
            let path = path.clone();
            Box::pin(async move {
                sqlx::query(&path).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    let queue = Queue::new(pool.clone());
    queue.migrate().await.unwrap();
    let server = server().await;
    let app = app(state(&server, queue.clone()));
    let user = Uuid::new_v4();
    let t = token(
        &claims(&server.url, user),
        Algorithm::ES256,
        Some("ec-test"),
    );
    let other = token(
        &claims(&server.url, Uuid::new_v4()),
        Algorithm::ES256,
        Some("ec-test"),
    );
    let input = include_str!("../../../tests/fixtures/en.json");
    let mut id = String::new();
    for count in [1, 5, 10] {
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..count {
            let app = app.clone();
            let t = t.clone();
            tasks.spawn(
                async move { call(app, "POST", "/api/translations", Some(&t), input).await },
            );
        }
        while let Some(response) = tasks.join_next().await {
            let (status, body) = response.unwrap();
            assert_eq!(status, StatusCode::ACCEPTED);
            let job = body["jobId"].as_str().unwrap();
            if id.is_empty() {
                id = job.to_owned();
            } else {
                assert_eq!(job, id);
            }
            assert_eq!(body["status"], "pending");
        }
    }
    let path = format!("/api/translations/{id}");
    assert_eq!(
        call(app.clone(), "GET", &path, Some(&other), "").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            &format!("/api/translations/{}", Uuid::new_v4()),
            Some(&t),
            ""
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(app.clone(), "GET", &path, Some(&t), "").await.1["status"],
        "pending"
    );
    let claimed = queue.claim("api-test-model").await.unwrap().unwrap();
    assert_eq!(
        call(app.clone(), "GET", &path, Some(&t), "").await.1["status"],
        "processing"
    );
    let translations = vec!["晨光".into(), "雨".into(), "".into(), "路".into()];
    queue
        .complete(
            &claimed,
            &shared::TranslationResult {
                translations: translations.clone(),
            },
        )
        .await
        .unwrap();
    let (status, body) = call(app.clone(), "GET", &path, Some(&t), "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["translations"], json!(translations));
    assert_eq!(body["status"], "completed");
    assert!(body.get("input_payload").is_none());
    assert_eq!(
        call(app.clone(), "POST", "/api/translations", Some(&t), input)
            .await
            .0,
        StatusCode::OK
    );
    let (_, other_job) = call(
        app.clone(),
        "POST",
        "/api/translations",
        Some(&other),
        input,
    )
    .await;
    assert_ne!(other_job["jobId"], id);
    for _ in 0..3 {
        let claimed = queue.claim("api-test-model").await.unwrap().unwrap();
        queue.fail(&claimed, false).await.unwrap();
    }
    assert_eq!(
        call(
            app,
            "GET",
            &format!("/api/translations/{}", other_job["jobId"].as_str().unwrap()),
            Some(&other),
            ""
        )
        .await
        .1["status"],
        "failed"
    );
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
}
