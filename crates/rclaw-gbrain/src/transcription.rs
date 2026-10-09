//! Audio transcription via Groq Whisper (default) or OpenAI Whisper
//! Mirrors gbrain's src/core/transcription.ts
//!
//! Supports:
//! - Groq Whisper API (default, fast)
//! - OpenAI Whisper API (fallback)
//! - Large file segmentation via ffmpeg (>25MB for Groq, >25MB for OpenAI)
//!
//! Provider selection via GBRAIN_TRANSCRIPTION_PROVIDER env var.

use crate::error::{GBrainError, Result};
use std::sync::OnceLock;
use tracing::{debug, info, warn};

/// Lazy-initialized HTTP client (reused across calls for connection pooling)
static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn get_http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(reqwest::Client::new)
}

/// Maximum file size in bytes before segmentation is required (25 MB)
const MAX_FILE_SIZE: u64 = 25 * 1024 * 1024;

/// Transcription provider
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptionProvider {
    Groq,
    OpenAI,
}

impl std::fmt::Display for TranscriptionProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Groq => write!(f, "groq"),
            Self::OpenAI => write!(f, "openai"),
        }
    }
}

impl std::str::FromStr for TranscriptionProvider {
    type Err = GBrainError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "groq" => Ok(Self::Groq),
            "openai" => Ok(Self::OpenAI),
            _ => Err(GBrainError::InvalidInput(format!(
                "Unknown transcription provider: {}. Supported: groq, openai",
                s
            ))),
        }
    }
}

/// Transcription result
#[derive(Debug, Clone)]
pub struct TranscriptionResult {
    pub text: String,
    pub provider: TranscriptionProvider,
    pub duration_secs: Option<f64>,
    pub language: Option<String>,
}

/// Transcribe an audio file using the configured provider.
///
/// If the file exceeds MAX_FILE_SIZE, it will be segmented via ffmpeg
/// (if available) and the segments transcribed individually.
pub async fn transcribe(
    file_path: &str,
    provider: &TranscriptionProvider,
    api_key: &str,
    base_url: &str,
    language: Option<&str>,
) -> Result<TranscriptionResult> {
    info!(file_path, provider = %provider, "Starting audio transcription");

    let path = std::path::Path::new(file_path);
    if !path.exists() {
        return Err(GBrainError::InvalidInput(format!(
            "Audio file not found: {}",
            file_path
        )));
    }

    let file_size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) => return Err(GBrainError::FileError(e.to_string())),
    };

    if file_size > MAX_FILE_SIZE {
        debug!(
            file_size,
            max_size = MAX_FILE_SIZE,
            "File exceeds size limit, segmenting via ffmpeg"
        );
        return transcribe_large_file(file_path, provider, api_key, base_url, language).await;
    }

    transcribe_single(file_path, provider, api_key, base_url, language).await
}

/// Transcribe a single file (under size limit)
async fn transcribe_single(
    file_path: &str,
    provider: &TranscriptionProvider,
    api_key: &str,
    base_url: &str,
    language: Option<&str>,
) -> Result<TranscriptionResult> {
    debug!(file_path, provider = %provider, base_url, "Transcribing single file");

    let client = get_http_client();
    let url = match provider {
        TranscriptionProvider::Groq => format!("{}/audio/transcriptions", base_url),
        TranscriptionProvider::OpenAI => format!("{}/audio/transcriptions", base_url),
    };

    let file_bytes = std::fs::read(file_path)?;
    let file_name = std::path::Path::new(file_path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let file_part = match reqwest::multipart::Part::bytes(file_bytes)
        .file_name(file_name)
        .mime_str("audio/mpeg")
    {
        Ok(part) => part,
        Err(_) => {
            // MIME type string was invalid (unlikely for "audio/mpeg").
            // Re-read file bytes for the fallback part since mime_str consumed the original.
            let fallback_bytes = std::fs::read(file_path).unwrap_or_default();
            reqwest::multipart::Part::bytes(fallback_bytes).file_name("audio.mp3")
        }
    };

    let mut form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("response_format", "verbose_json")
        .text("model", whisper_model(provider));

    if let Some(lang) = language {
        form = form.text("language", lang.to_string());
    }

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .multipart(form)
        .send()
        .await
        .map_err(|e| GBrainError::Transcription(format!("Request failed: {}", e)))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        warn!(status = status.as_u16(), "Transcription API error");
        return Err(GBrainError::Transcription(format!(
            "Transcription API error ({}): {}",
            status, body
        )));
    }

    let data: serde_json::Value = response
        .json()
        .await
        .map_err(|e| GBrainError::Transcription(format!("Failed to parse response: {}", e)))?;

    let text = data
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let duration = data.get("duration").and_then(|v| v.as_f64());

    let lang = data
        .get("language")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    info!(
        text_len = text.len(),
        duration = duration.unwrap_or(0.0),
        "Transcription complete"
    );

    Ok(TranscriptionResult {
        text,
        provider: provider.clone(),
        duration_secs: duration,
        language: lang,
    })
}

/// Transcribe a large file by segmenting with ffmpeg
async fn transcribe_large_file(
    file_path: &str,
    provider: &TranscriptionProvider,
    api_key: &str,
    base_url: &str,
    language: Option<&str>,
) -> Result<TranscriptionResult> {
    // Check if ffmpeg is available
    let ffmpeg_check = std::process::Command::new("ffmpeg")
        .arg("-version")
        .output();

    if ffmpeg_check.is_err() {
        return Err(GBrainError::Transcription(
            "File exceeds 25MB and ffmpeg is not available for segmentation".to_string(),
        ));
    }

    // Create unique temp directory for segments (avoids race conditions between concurrent transcriptions)
    let unique_id = {
        use std::time::SystemTime;
        let t = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        format!("{:x}", t.as_nanos())
    };
    let temp_dir = std::env::temp_dir().join(format!("gbrain_segments_{}", unique_id));
    std::fs::create_dir_all(&temp_dir)?;

    // Segment into 10-minute chunks
    let segment_pattern = temp_dir
        .join("segment_%03d.mp3")
        .to_string_lossy()
        .to_string();
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-i",
            file_path,
            "-f",
            "segment",
            "-segment_time",
            "600",
            "-c",
            "copy",
            &segment_pattern,
        ])
        .output()
        .map_err(|e| GBrainError::Transcription(format!("ffmpeg failed: {}", e)))?;

    if !output.status.success() {
        return Err(GBrainError::Transcription(
            "ffmpeg segmentation failed".to_string(),
        ));
    }

    // Transcribe each segment
    let mut full_text = String::new();
    let mut total_duration = 0.0;
    let mut detected_language: Option<String> = None;

    let mut segment_index = 0;
    loop {
        let segment_path = temp_dir.join(format!("segment_{:03}.mp3", segment_index));
        if !segment_path.exists() {
            break;
        }

        let path_str = segment_path.to_string_lossy().to_string();
        match transcribe_single(&path_str, provider, api_key, base_url, language).await {
            Ok(result) => {
                if !full_text.is_empty() {
                    full_text.push(' ');
                }
                full_text.push_str(&result.text);
                if let Some(d) = result.duration_secs {
                    total_duration += d;
                }
                if detected_language.is_none() {
                    detected_language = result.language;
                }
            }
            Err(e) => {
                warn!(segment_index, error = %e, "Failed to transcribe segment");
            }
        }

        // Clean up segment
        let _ = std::fs::remove_file(&segment_path);
        segment_index += 1;
    }

    // Clean up temp dir
    let _ = std::fs::remove_dir_all(&temp_dir);

    if full_text.is_empty() {
        return Err(GBrainError::Transcription(
            "No segments were successfully transcribed".to_string(),
        ));
    }

    info!(
        text_len = full_text.len(),
        total_duration,
        segments = segment_index,
        "Large file transcription complete"
    );

    Ok(TranscriptionResult {
        text: full_text,
        provider: provider.clone(),
        duration_secs: Some(total_duration),
        language: detected_language,
    })
}

/// Get the Whisper model name for a provider
fn whisper_model(provider: &TranscriptionProvider) -> String {
    match provider {
        TranscriptionProvider::Groq => "whisper-large-v3".to_string(),
        TranscriptionProvider::OpenAI => "whisper-1".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_provider_from_str() {
        assert_eq!(
            TranscriptionProvider::from_str("groq").unwrap(),
            TranscriptionProvider::Groq
        );
        assert_eq!(
            TranscriptionProvider::from_str("openai").unwrap(),
            TranscriptionProvider::OpenAI
        );
        assert_eq!(
            TranscriptionProvider::from_str("GROQ").unwrap(),
            TranscriptionProvider::Groq
        );
        assert!(TranscriptionProvider::from_str("unknown").is_err());
    }

    #[test]
    fn test_provider_display() {
        assert_eq!(TranscriptionProvider::Groq.to_string(), "groq");
        assert_eq!(TranscriptionProvider::OpenAI.to_string(), "openai");
    }

    #[test]
    fn test_whisper_model() {
        assert_eq!(
            whisper_model(&TranscriptionProvider::Groq),
            "whisper-large-v3"
        );
        assert_eq!(whisper_model(&TranscriptionProvider::OpenAI), "whisper-1");
    }
}
