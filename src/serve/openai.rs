use crate::tts::koko::TTSKoko;
use crate::utils::wav::{write_audio_chunk, WavHeader};
use axum::http::StatusCode;
use axum::{extract::State, routing::post, Json, Router};
use base64::Engine;
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;
use lame::Lame;

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum AudioFormat {
    Mp3,
    Wav,
}

impl Default for AudioFormat {
    fn default() -> Self {
        AudioFormat::Mp3
    }
}

#[derive(Deserialize)]
struct TTSRequest {
    #[allow(dead_code)]
    model: String,
    input: String,
    voice: Option<String>,
    #[serde(default = "default_true")]
    return_audio: bool,
    #[serde(default)]
    response_format: AudioFormat,
}

#[derive(Serialize)]
struct TTSResponse {
    status: String,
    file_path: Option<String>, // Made optional since we won't always have a file
    audio: Option<String>,     // Made optional since we won't always return audio
}

pub async fn create_server(tts: TTSKoko) -> Router {
    Router::new()
        .route("/v1/audio/speech", post(handle_tts))
        .layer(CorsLayer::permissive())
        .with_state(tts)
}

fn encode_to_mp3(raw_audio: &[f32]) -> Result<Vec<u8>, StatusCode> {
    let mut lame = Lame::new().expect("Failed to initialize LAME");
    lame.set_channels(1).expect("Failed to set channels");
    lame.set_sample_rate(TTSKoko::SAMPLE_RATE as i32).expect("Failed to set sample rate");
    lame.set_quality(3).expect("Failed to set quality"); // Quality range: 0 (best) to 9 (worst)
    lame.init_params().expect("Failed to initialize parameters");

    // Convert f32 samples to i16
    let pcm: Vec<i16> = raw_audio
        .iter()
        .map(|&x| (x * 32767.0) as i16)
        .collect();

    let mut mp3_data = Vec::new();
    let mut mp3_buffer = vec![0u8; pcm.len() * 2]; // Buffer size estimation

    let encoded = lame.encode(&pcm, &mut mp3_buffer)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    mp3_data.extend_from_slice(&mp3_buffer[..encoded]);

    // Flush the encoder
    let mut flush_buffer = vec![0u8; 7200];
    let flush_len = lame.flush(&mut flush_buffer)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    mp3_data.extend_from_slice(&flush_buffer[..flush_len]);

    Ok(mp3_data)
}

async fn handle_tts(
    State(tts): State<TTSKoko>,
    Json(payload): Json<TTSRequest>,
) -> Result<Json<TTSResponse>, StatusCode> {
    let voice = payload.voice.unwrap_or_else(|| "af_sky".to_string());
    let return_audio = payload.return_audio;

    match tts.tts_raw_audio(&payload.input, "en-us", &voice) {
        Ok(raw_audio) => {
            if return_audio {
                let encoded_data = match payload.response_format {
                    AudioFormat::Mp3 => encode_to_mp3(&raw_audio)?,
                    AudioFormat::Wav => {
                        let mut wav_data = Vec::new();
                        let header = WavHeader::new(1, TTSKoko::SAMPLE_RATE, 32);
                        header
                            .write_header(&mut wav_data)
                            .expect("Failed to write WAV header");
                        write_audio_chunk(&mut wav_data, &raw_audio).expect("Failed to write audio chunk");
                        wav_data
                    }
                };

                let audio_base64 = base64::engine::general_purpose::STANDARD.encode(&encoded_data);
                Ok(Json(TTSResponse {
                    status: "success".to_string(),
                    file_path: None,
                    audio: Some(audio_base64),
                }))
            } else {
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();

                let (output_path, encoded_data) = match payload.response_format {
                    AudioFormat::Mp3 => {
                        let path = format!("tmp/output_{}.mp3", timestamp);
                        let data = encode_to_mp3(&raw_audio)?;
                        (path, data)
                    },
                    AudioFormat::Wav => {
                        let path = format!("tmp/output_{}.wav", timestamp);
                        // Create WAV file
                        let spec = hound::WavSpec {
                            channels: 1,
                            sample_rate: TTSKoko::SAMPLE_RATE,
                            bits_per_sample: 32,
                            sample_format: hound::SampleFormat::Float,
                        };

                        let mut writer = hound::WavWriter::create(&path, spec)
                            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                        
                        for &sample in &raw_audio {
                            writer.write_sample(sample)
                                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                        }
                        writer.finalize()
                            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                        
                        (path, Vec::new()) // Empty vec since we write directly to file for WAV
                    }
                };

                // Write MP3 data to file if necessary
                if matches!(payload.response_format, AudioFormat::Mp3) {
                    std::fs::write(&output_path, encoded_data)
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                }

                Ok(Json(TTSResponse {
                    status: "success".to_string(),
                    file_path: Some(output_path),
                    audio: None,
                }))
            }
        }
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}
