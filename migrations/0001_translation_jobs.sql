CREATE TABLE translation_jobs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL,
    content_hash TEXT NOT NULL,
    source_language TEXT NOT NULL CHECK (source_language IN ('en', 'ja', 'ko')),
    target_language TEXT NOT NULL CHECK (target_language = 'zh-TW'),
    model_version TEXT NOT NULL,
    prompt_version TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','processing','completed','failed')),
    input_payload JSONB NOT NULL,
    result_payload JSONB,
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count BETWEEN 0 AND 3),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    lease_expires_at TIMESTAMPTZ,
    error_code TEXT CHECK (error_code IN ('inference_failed', 'job_timeout', 'lease_expired')),
    UNIQUE (user_id, content_hash, source_language, target_language, model_version, prompt_version),
    CHECK ((status = 'processing') = (lease_expires_at IS NOT NULL)),
    CHECK ((status IN ('completed', 'failed')) = (completed_at IS NOT NULL)),
    CHECK ((status = 'completed') = (result_payload IS NOT NULL))
);
CREATE INDEX translation_jobs_claim ON translation_jobs (model_version, prompt_version, created_at)
    WHERE status IN ('pending', 'processing');
CREATE INDEX translation_jobs_retention ON translation_jobs (created_at);
-- No access policies: direct frontend/PostgREST access must stay disabled.
ALTER TABLE translation_jobs ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON translation_jobs FROM PUBLIC;
