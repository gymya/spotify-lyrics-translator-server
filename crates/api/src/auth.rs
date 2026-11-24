use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{Jwk, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse},
};
use serde::Deserialize;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, PartialEq)]
pub enum AuthError {
    Unauthorized,
    Unavailable,
}
#[derive(Clone)]
pub struct Auth {
    issuer: String,
    jwks_url: String,
    client: reqwest::Client,
    cache: Arc<Mutex<Cache>>,
}
#[derive(Default)]
struct Cache {
    keys: Option<JwkSet>,
    fetched_at: Option<Instant>,
    attempted_at: Option<Instant>,
}
#[derive(Clone, Deserialize)]
struct Claims {
    sub: String,
    role: String,
    iat: u64,
    exp: u64,
}

impl Auth {
    pub fn new(supabase_url: &str) -> Result<Self, &'static str> {
        let mut url = reqwest::Url::parse(supabase_url).map_err(|_| "invalid SUPABASE_URL")?;
        let local = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        if (url.scheme() != "https" && !(url.scheme() == "http" && local))
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err(
                "SUPABASE_URL must be an HTTPS origin (HTTP allowed only on loopback), without credentials or a path",
            );
        }
        url.set_path("/auth/v1");
        let issuer = url.to_string();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "cannot initialize JWKS client")?;
        Ok(Self {
            jwks_url: format!("{issuer}/.well-known/jwks.json"),
            issuer,
            client,
            cache: Arc::new(Mutex::new(Cache::default())),
        })
    }

    pub async fn verify(&self, token: &str) -> Result<Uuid, AuthError> {
        if token.len() > 8192 {
            return Err(AuthError::Unauthorized);
        }
        let header = decode_header(token).map_err(|_| AuthError::Unauthorized)?;
        if !matches!(header.alg, Algorithm::ES256 | Algorithm::RS256) {
            return Err(AuthError::Unauthorized);
        }
        let kid = header
            .kid
            .filter(|kid| !kid.is_empty() && kid.len() <= 200)
            .ok_or(AuthError::Unauthorized)?;
        let jwk = self.key(&kid).await?;
        let expected = match header.alg {
            Algorithm::ES256 => KeyAlgorithm::ES256,
            Algorithm::RS256 => KeyAlgorithm::RS256,
            _ => unreachable!(),
        };
        if jwk.common.key_algorithm.is_some_and(|alg| alg != expected)
            || jwk
                .common
                .public_key_use
                .as_ref()
                .is_some_and(|usage| *usage != PublicKeyUse::Signature)
            || jwk
                .common
                .key_operations
                .as_ref()
                .is_some_and(|ops| !ops.contains(&KeyOperations::Verify))
        {
            return Err(AuthError::Unauthorized);
        }
        let key = DecodingKey::from_jwk(&jwk).map_err(|_| AuthError::Unauthorized)?;
        let mut validation = Validation::new(header.alg);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub", "iat"]);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&["authenticated"]);
        validation.validate_nbf = true;
        validation.leeway = 30;
        let claims = decode::<Claims>(token, &key, &validation)
            .map_err(|_| AuthError::Unauthorized)?
            .claims;
        if claims.role != "authenticated"
            || claims.iat > jsonwebtoken::get_current_timestamp() + 30
            || claims.iat >= claims.exp
        {
            return Err(AuthError::Unauthorized);
        }
        let user = Uuid::parse_str(&claims.sub).map_err(|_| AuthError::Unauthorized)?;
        if user.is_nil() {
            return Err(AuthError::Unauthorized);
        }
        Ok(user)
    }

    async fn key(&self, kid: &str) -> Result<Jwk, AuthError> {
        // One shared refresh and a cooldown bound traffic from attacker-controlled kid values.
        let mut cache = self.cache.lock().await;
        let fresh = cache
            .fetched_at
            .is_some_and(|at| at.elapsed() < Duration::from_secs(300));
        if fresh && let Some(key) = cache.keys.as_ref().and_then(|set| set.find(kid)) {
            return Ok(key.clone());
        }
        if cache
            .attempted_at
            .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
        {
            return Err(if fresh {
                AuthError::Unauthorized
            } else {
                AuthError::Unavailable
            });
        }
        cache.attempted_at = Some(Instant::now());
        let mut response = self
            .client
            .get(&self.jwks_url)
            .send()
            .await
            .map_err(|_| AuthError::Unavailable)?;
        if !response.status().is_success() {
            return Err(AuthError::Unavailable);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| AuthError::Unavailable)? {
            if body.len() + chunk.len() > 65536 {
                return Err(AuthError::Unavailable);
            }
            body.extend_from_slice(&chunk);
        }
        let keys: JwkSet = serde_json::from_slice(&body).map_err(|_| AuthError::Unavailable)?;
        let key = keys.find(kid).cloned();
        cache.keys = Some(keys);
        cache.fetched_at = Some(Instant::now());
        key.ok_or(AuthError::Unauthorized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use serde_json::json;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn verifies_es256_rs256_and_caches_jwks() {
        let server = server().await;
        let auth = Auth::new(&server.url).unwrap();
        let user = Uuid::new_v4();
        for (alg, kid) in [
            (Algorithm::ES256, "ec-test"),
            (Algorithm::RS256, "rsa-test"),
        ] {
            assert_eq!(
                auth.verify(&token(&claims(&server.url, user), alg, Some(kid)))
                    .await
                    .unwrap(),
                user
            );
        }
        assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rejects_invalid_claims_algorithms_and_signatures() {
        let server = server().await;
        let auth = Auth::new(&server.url).unwrap();
        let user = Uuid::new_v4();
        let valid = claims(&server.url, user);
        for (name, value) in [
            ("iss", json!("https://attacker.invalid/auth/v1")),
            ("aud", json!("other")),
            ("exp", json!(1)),
            ("nbf", json!(jsonwebtoken::get_current_timestamp() + 3600)),
            ("iat", json!(jsonwebtoken::get_current_timestamp() + 3600)),
            ("sub", json!("invalid")),
            ("sub", json!(Uuid::nil())),
            ("role", json!("service_role")),
        ] {
            let mut c = valid.clone();
            c[name] = value;
            assert_eq!(
                auth.verify(&token(&c, Algorithm::ES256, Some("ec-test")))
                    .await,
                Err(AuthError::Unauthorized),
                "{name}"
            );
        }
        for field in ["exp", "iat", "iss", "aud", "sub", "role"] {
            let mut c = valid.clone();
            c.as_object_mut().unwrap().remove(field);
            assert_eq!(
                auth.verify(&token(&c, Algorithm::ES256, Some("ec-test")))
                    .await,
                Err(AuthError::Unauthorized),
                "missing {field}"
            );
        }
        for t in [
            token(&valid, Algorithm::HS256, Some("ec-test")),
            token(&valid, Algorithm::ES256, None),
            token(&valid, Algorithm::ES256, Some("unknown")),
            "not.jwt".into(),
            "x".repeat(8193),
        ] {
            assert_eq!(auth.verify(&t).await, Err(AuthError::Unauthorized));
        }
        let t = token(&valid, Algorithm::ES256, Some("ec-test"));
        let (body, sig) = t.rsplit_once('.').unwrap();
        let changed = format!(
            "{body}.{}{}",
            if sig.starts_with('A') { "B" } else { "A" },
            &sig[1..]
        );
        assert_eq!(auth.verify(&changed).await, Err(AuthError::Unauthorized));
    }

    #[tokio::test]
    async fn rotates_keys_throttles_unknown_kids_and_fails_closed() {
        let server = server().await;
        let auth = Auth::new(&server.url).unwrap();
        let c = claims(&server.url, Uuid::new_v4());
        let old = token(&c, Algorithm::ES256, Some("ec-test"));
        auth.verify(&old).await.unwrap();
        let mut rotated = keys();
        rotated["keys"][0]["kid"] = json!("rotated");
        server.body.lock().unwrap().1 = rotated;
        let new = token(&c, Algorithm::ES256, Some("rotated"));
        assert_eq!(auth.verify(&new).await, Err(AuthError::Unauthorized));
        auth.cache.lock().await.attempted_at = Some(Instant::now() - Duration::from_secs(31));
        auth.verify(&new).await.unwrap();
        assert_eq!(auth.verify(&old).await, Err(AuthError::Unauthorized));
        for _ in 0..10 {
            assert_eq!(
                auth.verify(&token(&c, Algorithm::ES256, Some("attacker")))
                    .await,
                Err(AuthError::Unauthorized)
            );
        }
        assert_eq!(server.hits.load(Ordering::SeqCst), 2);
        *server.body.lock().unwrap() = (axum::http::StatusCode::SERVICE_UNAVAILABLE, json!({}));
        {
            let mut cache = auth.cache.lock().await;
            cache.fetched_at = Some(Instant::now() - Duration::from_secs(301));
            cache.attempted_at = Some(Instant::now() - Duration::from_secs(31));
        }
        assert_eq!(auth.verify(&new).await, Err(AuthError::Unavailable));
        assert_eq!(auth.verify(&new).await, Err(AuthError::Unavailable));
        assert_eq!(server.hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn rejects_jwk_algorithm_and_usage_mismatches() {
        for (field, value) in [
            ("alg", json!("RS256")),
            ("use", json!("enc")),
            ("key_ops", json!(["encrypt"])),
        ] {
            let server = server().await;
            server.body.lock().unwrap().1["keys"][0][field] = value;
            let auth = Auth::new(&server.url).unwrap();
            assert_eq!(
                auth.verify(&token(
                    &claims(&server.url, Uuid::new_v4()),
                    Algorithm::ES256,
                    Some("ec-test")
                ))
                .await,
                Err(AuthError::Unauthorized)
            );
        }
    }

    #[test]
    fn validates_trusted_issuer_configuration() {
        for url in [
            "http://example.com",
            "https://user:pass@example.com",
            "https://example.com/path",
            "https://example.com?x=1",
            "file:///tmp",
        ] {
            assert!(Auth::new(url).is_err());
        }
        assert!(Auth::new("https://project.supabase.co").is_ok());
        assert!(Auth::new("http://127.0.0.1:54321").is_ok());
    }
}
