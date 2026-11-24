use shared::{
    TranslationRequest, TranslationResult,
    queue::{PROMPT_VERSION, Queue},
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::collections::HashSet;
use uuid::Uuid;

fn input(text: &str) -> TranslationRequest {
    TranslationRequest {
        lyrics: vec![text.into(), "".into()],
        source_language: "en".into(),
        target_language: "zh-TW".into(),
    }
}
fn result() -> TranslationResult {
    TranslationResult {
        translations: vec!["你好".into(), "".into()],
    }
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL; creates and drops an isolated schema"]
async fn postgres_queue_concurrency_recovery_and_retention() {
    let url = std::env::var("TEST_DATABASE_URL").expect("set TEST_DATABASE_URL");
    let admin = PgPool::connect(&url).await.unwrap();
    let schema = format!("test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let search_path = format!("SET search_path TO {schema}");
    let pool = PgPoolOptions::new()
        .max_connections(15)
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
    queue.migrate().await.unwrap();

    for concurrency in [1, 5, 10] {
        let model = format!("test-{concurrency}");
        let user = Uuid::new_v4();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..concurrency {
            let q = queue.clone();
            let model = model.clone();
            tasks.spawn(async move {
                q.enqueue(user, &input("same content"), &model)
                    .await
                    .unwrap()
                    .job_id
            });
        }
        let mut ids = HashSet::new();
        while let Some(id) = tasks.join_next().await {
            ids.insert(id.unwrap());
        }
        assert_eq!(
            ids.len(),
            1,
            "concurrent duplicate inserts must return one job"
        );
        let id = *ids.iter().next().unwrap();
        assert!(queue.get(Uuid::new_v4(), id).await.unwrap().is_none());
        for i in 1..concurrency {
            queue
                .enqueue(user, &input(&format!("line {i}")), &model)
                .await
                .unwrap();
        }
        for _ in 0..concurrency {
            let q = queue.clone();
            let model = model.clone();
            tasks.spawn(async move { q.claim(&model).await.unwrap().unwrap().id });
        }
        let mut claimed = HashSet::new();
        while let Some(id) = tasks.join_next().await {
            assert!(claimed.insert(id.unwrap()));
        }
        assert_eq!(claimed.len(), concurrency);
        assert!(queue.claim(&model).await.unwrap().is_none());
        assert_ne!(
            queue
                .enqueue(Uuid::new_v4(), &input("same content"), &model)
                .await
                .unwrap()
                .job_id,
            id
        );
    }

    let user = Uuid::new_v4();
    let first = queue
        .enqueue(user, &input("recover"), "recovery")
        .await
        .unwrap();
    let old = queue.claim("recovery").await.unwrap().unwrap();
    sqlx::query(
        "UPDATE translation_jobs SET lease_expires_at=now()-interval '1 second' WHERE id=$1",
    )
    .bind(old.id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(!queue.complete(&old, &result()).await.unwrap());
    let new = queue.claim("recovery").await.unwrap().unwrap();
    assert_eq!(new.id, old.id);
    assert_eq!(new.attempt, 2);
    assert!(!queue.fail(&old, false).await.unwrap());
    assert!(!queue.complete(&old, &result()).await.unwrap());
    assert!(queue.complete(&new, &result()).await.unwrap());
    let done = queue.get(user, first.job_id).await.unwrap().unwrap();
    assert_eq!(done.status, "completed");
    assert_eq!(done.translations, Some(result().translations));
    assert_eq!(
        queue
            .enqueue(user, &input("recover"), "recovery")
            .await
            .unwrap()
            .job_id,
        first.job_id
    );
    assert_ne!(
        queue
            .enqueue(user, &input("recover"), "new-model")
            .await
            .unwrap()
            .job_id,
        first.job_id
    );
    sqlx::query("UPDATE translation_jobs SET prompt_version='old' WHERE id=$1")
        .bind(first.job_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_ne!(
        queue
            .enqueue(user, &input("recover"), "recovery")
            .await
            .unwrap()
            .job_id,
        first.job_id
    );

    for model in ["fail", "crash"] {
        let job = queue.enqueue(user, &input(model), model).await.unwrap();
        for attempt in 1..=3 {
            let claimed = queue.claim(model).await.unwrap().unwrap();
            assert_eq!(claimed.attempt, attempt);
            if model == "fail" {
                assert!(queue.fail(&claimed, true).await.unwrap());
            } else {
                sqlx::query("UPDATE translation_jobs SET lease_expires_at=now()-interval '1 second' WHERE id=$1").bind(claimed.id).execute(&pool).await.unwrap();
            }
        }
        assert!(queue.claim(model).await.unwrap().is_none());
        assert_eq!(
            queue.get(user, job.job_id).await.unwrap().unwrap().status,
            "failed"
        );
    }
    let active = queue
        .enqueue(user, &input("keep"), "retention")
        .await
        .unwrap();
    queue.claim("retention").await.unwrap().unwrap();
    sqlx::query("UPDATE translation_jobs SET created_at=now()-interval '8 days' WHERE id=ANY($1)")
        .bind(vec![first.job_id, active.job_id])
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(queue.cleanup().await.unwrap(), 1);
    assert!(queue.get(user, first.job_id).await.unwrap().is_none());
    assert!(queue.get(user, active.job_id).await.unwrap().is_some());
    assert_eq!(queue.cleanup().await.unwrap(), 0);
    assert!(queue.enqueue(user, &input("valid"), "").await.is_err());
    let mut bad = input("bad");
    bad.source_language = "fr".into();
    assert!(queue.enqueue(user, &bad, "invalid").await.is_err());
    assert!(!PROMPT_VERSION.is_empty());

    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
}
