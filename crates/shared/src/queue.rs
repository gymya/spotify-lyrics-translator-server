use crate::{TranslationRequest, TranslationResult};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{
    PgPool, Row,
    postgres::{PgPoolOptions, PgRow},
};
use std::time::Duration;
use uuid::Uuid;

pub const PROMPT_VERSION: &str = "lyrics-json-v1";
pub const MAX_JOB_ATTEMPTS: i32 = 3;
pub const JOB_TIMEOUT_SECONDS: u64 = 600;
pub const LEASE_SECONDS: i32 = 630;
pub const RETENTION_DAYS: i32 = 7;

// Never expose raw database errors: PostgreSQL errors can include row contents.
#[derive(Debug)]
pub enum QueueError {
    Database,
    InvalidInput,
    InvalidResult,
}
impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for QueueError {}
impl From<sqlx::Error> for QueueError {
    fn from(_: sqlx::Error) -> Self {
        Self::Database
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub job_id: Uuid,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub translations: Option<Vec<String>>,
}

pub struct ClaimedJob {
    pub id: Uuid,
    pub attempt: i32,
    pub input: TranslationRequest,
}

#[derive(Clone)]
pub struct Queue {
    pool: PgPool,
}
impl Queue {
    pub async fn connect(url: &str) -> Result<Self, QueueError> {
        let pool = PgPoolOptions::new()
            .max_connections(12)
            .acquire_timeout(Duration::from_secs(5))
            .connect(url)
            .await?;
        Ok(Self { pool })
    }
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
    pub async fn migrate(&self) -> Result<(), QueueError> {
        sqlx::migrate!("../../migrations")
            .run(&self.pool)
            .await
            .map_err(|_| QueueError::Database)
    }
    pub async fn enqueue(
        &self,
        user: Uuid,
        input: &TranslationRequest,
        model: &str,
    ) -> Result<Job, QueueError> {
        input.validate().map_err(|_| QueueError::InvalidInput)?;
        if model.trim().is_empty() || model.len() > 200 {
            return Err(QueueError::InvalidInput);
        }
        let payload = serde_json::to_value(input).map_err(|_| QueueError::InvalidInput)?;
        let hash = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&input.lyrics).map_err(|_| QueueError::InvalidInput)?
            )
        );
        let row = sqlx::query("INSERT INTO translation_jobs (user_id, content_hash, source_language, target_language, model_version, prompt_version, input_payload) VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (user_id,content_hash,source_language,target_language,model_version,prompt_version) DO UPDATE SET content_hash = EXCLUDED.content_hash RETURNING id,status,result_payload")
            .bind(user).bind(hash).bind(&input.source_language).bind(&input.target_language).bind(model).bind(PROMPT_VERSION).bind(payload)
            .fetch_one(&self.pool).await?;
        job_from_row(row)
    }
    pub async fn get(&self, user: Uuid, id: Uuid) -> Result<Option<Job>, QueueError> {
        sqlx::query(
            "SELECT id,status,result_payload FROM translation_jobs WHERE id=$1 AND user_id=$2",
        )
        .bind(id)
        .bind(user)
        .fetch_optional(&self.pool)
        .await?
        .map(job_from_row)
        .transpose()
    }
    pub async fn claim(&self, model: &str) -> Result<Option<ClaimedJob>, QueueError> {
        sqlx::query("UPDATE translation_jobs SET status='failed',completed_at=now(),lease_expires_at=NULL,error_code='lease_expired' WHERE status='processing' AND lease_expires_at <= now() AND attempt_count >= $1")
            .bind(MAX_JOB_ATTEMPTS).execute(&self.pool).await?;
        let row = sqlx::query("WITH candidate AS (SELECT id FROM translation_jobs WHERE model_version=$1 AND prompt_version=$2 AND attempt_count < $3 AND (status='pending' OR (status='processing' AND lease_expires_at <= now())) ORDER BY created_at,id FOR UPDATE SKIP LOCKED LIMIT 1) UPDATE translation_jobs j SET status='processing',attempt_count=j.attempt_count+1,started_at=now(),lease_expires_at=now()+make_interval(secs => $4),error_code=NULL FROM candidate c WHERE j.id=c.id RETURNING j.id,j.attempt_count,j.input_payload")
            .bind(model).bind(PROMPT_VERSION).bind(MAX_JOB_ATTEMPTS).bind(f64::from(LEASE_SECONDS))
            .fetch_optional(&self.pool).await?;
        row.map(|row| {
            Ok(ClaimedJob {
                id: row.try_get("id")?,
                attempt: row.try_get("attempt_count")?,
                input: serde_json::from_value(row.try_get("input_payload")?)
                    .map_err(|_| QueueError::InvalidInput)?,
            })
        })
        .transpose()
    }
    pub async fn complete(
        &self,
        job: &ClaimedJob,
        result: &TranslationResult,
    ) -> Result<bool, QueueError> {
        if result.translations.len() != job.input.lyrics.len()
            || result
                .translations
                .iter()
                .zip(&job.input.lyrics)
                .any(|(out, input)| {
                    out.contains(['\n', '\r']) || (out.trim().is_empty() != input.trim().is_empty())
                })
        {
            return Err(QueueError::InvalidResult);
        }
        let result = serde_json::to_value(result).map_err(|_| QueueError::InvalidResult)?;
        Ok(sqlx::query("UPDATE translation_jobs SET status='completed',result_payload=$3,completed_at=now(),lease_expires_at=NULL,error_code=NULL WHERE id=$1 AND attempt_count=$2 AND status='processing' AND lease_expires_at > now()")
            .bind(job.id).bind(job.attempt).bind(result).execute(&self.pool).await?.rows_affected() == 1)
    }
    pub async fn fail(&self, job: &ClaimedJob, timed_out: bool) -> Result<bool, QueueError> {
        Ok(sqlx::query("UPDATE translation_jobs SET status=CASE WHEN attempt_count >= $3 THEN 'failed' ELSE 'pending' END,completed_at=CASE WHEN attempt_count >= $3 THEN now() ELSE NULL END,lease_expires_at=NULL,error_code=$4 WHERE id=$1 AND attempt_count=$2 AND status='processing' AND lease_expires_at > now()")
            .bind(job.id).bind(job.attempt).bind(MAX_JOB_ATTEMPTS).bind(if timed_out {"job_timeout"} else {"inference_failed"}).execute(&self.pool).await?.rows_affected() == 1)
    }
    pub async fn cleanup(&self) -> Result<u64, QueueError> {
        // Delete in bounded batches and never remove an active lease.
        Ok(sqlx::query("WITH expired AS (SELECT id FROM translation_jobs WHERE created_at < now()-make_interval(days => $1) AND (status <> 'processing' OR lease_expires_at <= now()) ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT 1000) DELETE FROM translation_jobs j USING expired e WHERE j.id=e.id")
            .bind(RETENTION_DAYS).execute(&self.pool).await?.rows_affected())
    }
}
fn job_from_row(row: PgRow) -> Result<Job, QueueError> {
    let payload: Option<serde_json::Value> = row.try_get("result_payload")?;
    let result = payload
        .map(serde_json::from_value::<TranslationResult>)
        .transpose()
        .map_err(|_| QueueError::InvalidResult)?;
    Ok(Job {
        job_id: row.try_get("id")?,
        status: row.try_get("status")?,
        translations: result.map(|r| r.translations),
    })
}
