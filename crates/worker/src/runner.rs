use crate::Ollama;
use shared::queue::{JOB_TIMEOUT_SECONDS, Queue, QueueError};
use std::time::Duration;

pub async fn process_one(queue: &Queue, ollama: &Ollama) -> Result<bool, QueueError> {
    let Some(job) = queue.claim(&ollama.model).await? else {
        return Ok(false);
    };
    let result = tokio::time::timeout(
        Duration::from_secs(JOB_TIMEOUT_SECONDS),
        ollama.translate(&job.input),
    )
    .await;
    let saved = match result {
        Ok(Ok(result)) => queue.complete(&job, &result).await?,
        Ok(Err(_)) => queue.fail(&job, false).await?,
        Err(_) => queue.fail(&job, true).await?,
    };
    tracing::info!(job_id = %job.id, attempt = job.attempt, saved, "job attempt finished");
    Ok(true)
}

pub async fn run(queue: Queue, ollama: Ollama) {
    let work = async {
        let mut cleanup_at = tokio::time::Instant::now();
        loop {
            if tokio::time::Instant::now() >= cleanup_at {
                match queue.cleanup().await {
                    Ok(deleted) => tracing::info!(deleted, "retention cleanup completed"),
                    Err(error) => tracing::warn!(%error, "retention cleanup failed"),
                }
                cleanup_at = tokio::time::Instant::now() + Duration::from_secs(60);
            }
            match process_one(&queue, &ollama).await {
                Ok(true) => {}
                Ok(false) => tokio::time::sleep(Duration::from_secs(1)).await,
                Err(error) => {
                    tracing::warn!(%error, "queue operation failed");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    };
    tokio::select! { _ = work => {}, _ = shared::shutdown_signal() => {
        // Keep the lease: an in-flight remote inference may still be running.
        tracing::info!("worker stopped; unfinished job recoverable after lease expiry");
    } }
}
