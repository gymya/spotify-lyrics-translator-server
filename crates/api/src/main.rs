use api::{AppState, app, auth::Auth};
use shared::queue::Queue;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("--healthcheck") {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .no_proxy()
            .build()?
            .get("http://127.0.0.1:3000/health")
            .send()
            .await?
            .error_for_status()?;
        return Ok(());
    }
    shared::init_logging();
    let supabase = std::env::var("SUPABASE_URL").map_err(|_| "set SUPABASE_URL")?;
    let database = std::env::var("DATABASE_URL").map_err(|_| "set DATABASE_URL")?;
    let model =
        std::env::var("OLLAMA_MODEL").map_err(|_| "set OLLAMA_MODEL (must match Worker)")?;
    if model.trim().is_empty() || model.len() > 200 {
        return Err("invalid OLLAMA_MODEL".into());
    }
    let auth = Auth::new(&supabase)?;
    let queue = Queue::connect(&database).await?;
    let address = std::env::var("API_BIND").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let listener = tokio::net::TcpListener::bind(&address).await?;
    tracing::info!(%address, "API listening");
    axum::serve(listener, app(AppState { queue, auth, model }))
        .with_graceful_shutdown(shared::shutdown_signal())
        .await?;
    Ok(())
}
