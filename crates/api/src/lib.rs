pub mod auth;
use auth::{Auth, AuthError};
use axum::{
    Extension, Json, Router,
    extract::{
        DefaultBodyLimit, Path, Request, State,
        rejection::{JsonRejection, PathRejection},
    },
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;
use shared::{
    TranslationRequest,
    queue::{Job, Queue, QueueError},
};
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub queue: Queue,
    pub auth: Auth,
    pub model: String,
}

pub struct ApiError(StatusCode, &'static str, &'static str);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.0,
            Json(json!({"error":{"code":self.1,"message":self.2}})),
        )
            .into_response();
        if self.0 == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, "Bearer".parse().unwrap());
        }
        response
    }
}
impl From<QueueError> for ApiError {
    fn from(error: QueueError) -> Self {
        match error {
            QueueError::InvalidInput => Self(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid translation request",
            ),
            _ => {
                tracing::warn!(%error,"database operation failed");
                Self(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "service_unavailable",
                    "Translation service temporarily unavailable",
                )
            }
        }
    }
}
impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::Unauthorized => Self(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "A valid access token is required",
            ),
            AuthError::Unavailable => Self(
                StatusCode::SERVICE_UNAVAILABLE,
                "auth_unavailable",
                "Authentication service temporarily unavailable",
            ),
        }
    }
}

pub fn app(state: AppState) -> Router {
    let secured = Router::new()
        .route("/api/translations", post(create))
        .route("/api/translations/{jobId}", get(fetch))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .route("/health", get(|| async { Json(json!({"status":"ok"})) }))
        .merge(secured)
        .fallback(|| async { ApiError(StatusCode::NOT_FOUND, "not_found", "Route not found") })
        .method_not_allowed_fallback(|| async {
            ApiError(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "Method not allowed",
            )
        })
        .layer(DefaultBodyLimit::max(65536))
        .layer(middleware::from_fn(response_headers))
        .with_state(state)
}
async fn response_headers(request: Request, next: Next) -> Response {
    let mut response =
        match tokio::time::timeout(std::time::Duration::from_secs(20), next.run(request)).await {
            Ok(response) => response,
            Err(_) => ApiError(
                StatusCode::GATEWAY_TIMEOUT,
                "request_timeout",
                "Request timed out",
            )
            .into_response(),
        };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}
async fn authenticate(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let values = request.headers().get_all(header::AUTHORIZATION);
    let mut values = values.iter();
    let value = values
        .next()
        .and_then(|v| v.to_str().ok())
        .ok_or(AuthError::Unauthorized)?;
    if values.next().is_some() {
        return Err(AuthError::Unauthorized.into());
    }
    let (scheme, token) = value.split_once(' ').ok_or(AuthError::Unauthorized)?;
    if !scheme.eq_ignore_ascii_case("Bearer")
        || token.is_empty()
        || token.contains(char::is_whitespace)
    {
        return Err(AuthError::Unauthorized.into());
    }
    let user = state.auth.verify(token).await?;
    request.extensions_mut().insert(user);
    Ok(next.run(request).await)
}
async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<Uuid>,
    input: Result<Json<TranslationRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Job>), ApiError> {
    let Json(input) = input.map_err(|err| {
        if err.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "Request exceeds 64 KiB",
            )
        } else {
            ApiError(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Expected a valid translation JSON request",
            )
        }
    })?;
    input
        .validate()
        .map_err(|message| ApiError(StatusCode::BAD_REQUEST, "invalid_request", message))?;
    let job = state.queue.enqueue(user, &input, &state.model).await?;
    let status = if matches!(job.status.as_str(), "completed" | "failed") {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((status, Json(job)))
}
async fn fetch(
    State(state): State<AppState>,
    Extension(user): Extension<Uuid>,
    path: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<Job>, ApiError> {
    let Path(id) = path.map_err(|_| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "invalid_job_id",
            "jobId must be a UUID",
        )
    })?;
    let job = state.queue.get(user, id).await?.ok_or(ApiError(
        StatusCode::NOT_FOUND,
        "not_found",
        "Job not found",
    ))?;
    Ok(Json(job))
}

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
