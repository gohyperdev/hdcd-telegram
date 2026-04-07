// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Maciej Ostaszewski / HyperDev P.S.A.

//! Voice message transcription via the `whisper` CLI.
//!
//! Checks for `whisper` + `ffmpeg` at startup and provides an async
//! transcription function that converts OGG/OGA voice files to text.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

/// Result of the startup dependency check.
#[derive(Debug, Clone)]
pub struct TranscribeSupport {
    pub available: bool,
    pub whisper_path: Option<String>,
    pub ffmpeg_path: Option<String>,
}

/// Transcription configuration read from environment variables.
#[derive(Debug, Clone)]
pub struct TranscribeConfig {
    pub model: String,
    pub language: Option<String>,
    pub echo_transcript: bool,
}

/// Allowed whisper model names.
const ALLOWED_WHISPER_MODELS: &[&str] = &[
    "tiny", "base", "small", "medium", "large", "large-v2", "large-v3",
];

impl TranscribeConfig {
    /// Read configuration from environment variables.
    pub fn from_env() -> Self {
        let raw_model = std::env::var("WHISPER_MODEL").unwrap_or_else(|_| "small".into());
        let model = if ALLOWED_WHISPER_MODELS.contains(&raw_model.as_str()) {
            raw_model
        } else {
            warn!(
                requested = %raw_model,
                fallback = "small",
                "WHISPER_MODEL is not a recognised model name, falling back to 'small'"
            );
            "small".into()
        };

        let language = std::env::var("WHISPER_LANGUAGE")
            .ok()
            .filter(|s| !s.is_empty())
            .and_then(|s| {
                if s.chars().all(|c| c.is_ascii_alphabetic()) {
                    Some(s)
                } else {
                    warn!(
                        value = %s,
                        "WHISPER_LANGUAGE contains non-alphabetic characters, ignoring"
                    );
                    None
                }
            });

        let echo_transcript = std::env::var("HDCD_ECHO_TRANSCRIPT")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);
        Self {
            model,
            language,
            echo_transcript,
        }
    }
}

/// Check whether `whisper` and `ffmpeg` are available in PATH.
pub fn check_transcribe_support() -> TranscribeSupport {
    let whisper_path = find_executable("whisper");
    let ffmpeg_path = find_executable("ffmpeg");

    let available = whisper_path.is_some() && ffmpeg_path.is_some();

    if available {
        info!(
            whisper = whisper_path.as_deref().unwrap_or("?"),
            ffmpeg = ffmpeg_path.as_deref().unwrap_or("?"),
            "voice transcription enabled"
        );
    } else {
        warn!(
            whisper = ?whisper_path,
            ffmpeg = ?ffmpeg_path,
            "voice transcription disabled: whisper/ffmpeg not found"
        );
    }

    TranscribeSupport {
        available,
        whisper_path,
        ffmpeg_path,
    }
}

/// Locate an executable in PATH.
fn find_executable(name: &str) -> Option<String> {
    #[cfg(unix)]
    let which_cmd = "which";
    #[cfg(windows)]
    let which_cmd = "where";

    let output = std::process::Command::new(which_cmd)
        .arg(name)
        .output()
        .ok()?;

    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if path.is_empty() {
            None
        } else {
            // Take first line (in case `where` returns multiple)
            Some(path.lines().next().unwrap_or(&path).to_string())
        }
    } else {
        None
    }
}

/// Transcribe an OGG/OGA voice file to text using whisper CLI.
///
/// 1. Converts the input to 16 kHz mono WAV via ffmpeg.
/// 2. Runs whisper on the WAV file.
/// 3. Reads the resulting `.txt` file.
/// 4. Cleans up temp files.
///
/// Timeout: 60 seconds per step.
pub async fn transcribe(ogg_path: &Path, config: &TranscribeConfig) -> Result<String> {
    let temp_dir = std::env::temp_dir();
    let id = uuid::Uuid::new_v4();
    let wav_path = temp_dir.join(format!("hdcd-voice-{id}.wav"));
    let txt_path = temp_dir.join(format!("hdcd-voice-{id}.txt"));

    // Ensure cleanup on all exit paths.
    let result = transcribe_inner(ogg_path, &wav_path, &txt_path, &temp_dir, id, config).await;

    // Clean up temp files regardless of success or failure.
    cleanup(&[&wav_path, &txt_path]).await;

    result
}

async fn transcribe_inner(
    ogg_path: &Path,
    wav_path: &Path,
    txt_path: &Path,
    temp_dir: &Path,
    id: uuid::Uuid,
    config: &TranscribeConfig,
) -> Result<String> {
    let timeout = Duration::from_secs(60);

    // Step 1: Convert OGG to WAV via ffmpeg.
    let ffmpeg = tokio::time::timeout(
        timeout,
        tokio::process::Command::new("ffmpeg")
            .args([
                "-i",
                &ogg_path.to_string_lossy(),
                "-ar",
                "16000",
                "-ac",
                "1",
                &wav_path.to_string_lossy(),
                "-y",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("spawn ffmpeg")?
            .wait_with_output(),
    )
    .await
    .context("ffmpeg timed out (60s)")?
    .context("ffmpeg execution failed")?;

    if !ffmpeg.status.success() {
        let stderr = String::from_utf8_lossy(&ffmpeg.stderr);
        bail!("ffmpeg conversion failed: {stderr}");
    }

    // Step 2: Run whisper.
    let mut whisper_args = vec![
        wav_path.to_string_lossy().into_owned(),
        "--model".into(),
        config.model.clone(),
        "--output_format".into(),
        "txt".into(),
        "--output_dir".into(),
        temp_dir.to_string_lossy().into_owned(),
    ];
    if let Some(ref lang) = config.language {
        whisper_args.push("--language".into());
        whisper_args.push(lang.clone());
    }

    let whisper = tokio::time::timeout(
        timeout,
        tokio::process::Command::new("whisper")
            .args(&whisper_args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("spawn whisper")?
            .wait_with_output(),
    )
    .await
    .context("whisper timed out (60s)")?
    .context("whisper execution failed")?;

    if !whisper.status.success() {
        let stderr = String::from_utf8_lossy(&whisper.stderr);
        bail!("whisper transcription failed: {stderr}");
    }

    // Step 3: Read the output text file.
    // whisper names the output after the input file stem.
    let expected_name = format!("hdcd-voice-{id}.txt");
    let expected_path = temp_dir.join(&expected_name);

    let text = if expected_path.exists() {
        tokio::fs::read_to_string(&expected_path)
            .await
            .with_context(|| format!("read whisper output {}", expected_path.display()))?
    } else if txt_path.exists() {
        tokio::fs::read_to_string(txt_path)
            .await
            .with_context(|| format!("read whisper output {}", txt_path.display()))?
    } else {
        // Try to find any txt file whisper may have created with a different name pattern.
        bail!(
            "whisper output file not found at {} or {}",
            expected_path.display(),
            txt_path.display()
        );
    };

    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        bail!("whisper returned empty transcription");
    }

    Ok(trimmed)
}

/// Remove temp files, ignoring errors.
async fn cleanup(paths: &[&Path]) {
    for path in paths {
        let _ = tokio::fs::remove_file(path).await;
    }
}
