use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use uuid::Uuid;

type MockState = (Arc<Mutex<(StatusCode, Value)>>, Arc<AtomicUsize>);

pub struct JwksServer {
    pub url: String,
    pub body: Arc<Mutex<(StatusCode, Value)>>,
    pub hits: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for JwksServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub fn keys() -> Value {
    serde_json::from_str(include_str!("../tests/fixtures/jwks.json")).unwrap()
}
pub async fn server() -> JwksServer {
    let body = Arc::new(Mutex::new((StatusCode::OK, keys())));
    let hits = Arc::new(AtomicUsize::new(0));
    let state = (body.clone(), hits.clone());
    async fn handler(State((body, hits)): State<MockState>) -> (StatusCode, Json<Value>) {
        hits.fetch_add(1, Ordering::SeqCst);
        let (status, value) = body.lock().unwrap().clone();
        (status, Json(value))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/auth/v1/.well-known/jwks.json", get(handler))
        .with_state(state);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    JwksServer {
        url,
        body,
        hits,
        task,
    }
}
pub fn claims(url: &str, user: Uuid) -> Value {
    let now = jsonwebtoken::get_current_timestamp();
    json!({"iss":format!("{url}/auth/v1"),"aud":"authenticated","sub":user.to_string(),"role":"authenticated","iat":now,"exp":now+3600})
}
pub fn token(claims: &Value, alg: Algorithm, kid: Option<&str>) -> String {
    let mut header = Header::new(alg);
    header.kid = kid.map(str::to_owned);
    let key = match alg {
        Algorithm::ES256 => {
            EncodingKey::from_ec_pem(include_bytes!("../tests/fixtures/es256-test-only.pem"))
                .unwrap()
        }
        Algorithm::RS256 => {
            EncodingKey::from_rsa_pem(include_bytes!("../tests/fixtures/rs256-test-only.pem"))
                .unwrap()
        }
        Algorithm::HS256 => EncodingKey::from_secret(b"untrusted-secret"),
        _ => unreachable!(),
    };
    encode(&header, claims, &key).unwrap()
}
