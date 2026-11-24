pub mod queue;

use serde::{Deserialize, Serialize};

pub const MAX_INPUT_BYTES: usize = 16 * 1024;
pub const MAX_LINES: usize = 200;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TranslationRequest {
    pub lyrics: Vec<String>,
    pub source_language: String,
    pub target_language: String,
}

impl TranslationRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !matches!(self.source_language.as_str(), "en" | "ja" | "ko") {
            return Err("sourceLanguage must be en, ja or ko");
        }
        if self.target_language != "zh-TW" {
            return Err("targetLanguage must be zh-TW");
        }
        if self.lyrics.is_empty() || self.lyrics.len() > MAX_LINES {
            return Err("lyrics must contain 1 to 200 lines");
        }
        if self.lyrics.iter().map(String::len).sum::<usize>() > MAX_INPUT_BYTES {
            return Err("lyrics exceed 16384 bytes");
        }
        if self.lyrics.iter().all(|s| s.trim().is_empty()) {
            return Err("lyrics must contain text");
        }
        if self.lyrics.iter().any(|s| s.contains(['\n', '\r'])) {
            return Err("each lyrics item must be a single line");
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TranslationResult {
    pub translations: Vec<String>,
}

pub fn init_logging() {
    // Fixed application filter: HTTP debug logging can expose request bodies or secrets.
    tracing_subscriber::fmt()
        .with_env_filter("api=info,worker=info")
        .with_writer(std::io::stderr)
        .init();
}

pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_languages_and_limits() {
        let mut r = TranslationRequest {
            lyrics: vec!["Hello".into(), "".into()],
            source_language: "en".into(),
            target_language: "zh-TW".into(),
        };
        for lang in ["en", "ja", "ko"] {
            r.source_language = lang.into();
            assert!(r.validate().is_ok());
        }
        r.source_language = "fr".into();
        assert!(r.validate().is_err());
        r.source_language = "en".into();
        r.target_language = "zh-CN".into();
        assert!(r.validate().is_err());
        r.target_language = "zh-TW".into();
        for lines in [
            vec![],
            vec!["".into()],
            vec!["a\nb".into()],
            vec!["a".into(); 201],
            vec!["a".repeat(MAX_INPUT_BYTES + 1)],
        ] {
            r.lyrics = lines;
            assert!(r.validate().is_err());
        }
    }
}
