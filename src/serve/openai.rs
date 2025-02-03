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

// Add our own FFI bindings for LAME's flush function.
// We define a dummy type for the underlying C type.
#[repr(C)]
struct lame_global_flags {
    _private: [u8; 0],
}

// Alias for the LAME handle
type lame_t = lame_global_flags;

extern "C" {
    // Declaration for the native function:
    // int lame_encode_flush(lame_t *gfp, unsigned char *mp3buf, int size);
    fn lame_encode_flush(lame: *mut lame_t, mp3buf: *mut u8, size: i32) -> i32;
}

fn encode_to_mp3(raw_audio: &[f32]) -> Result<Vec<u8>, StatusCode> {
    let mut lame = Lame::new().expect("Failed to initialize LAME");
    lame.set_channels(1).expect("Failed to set channels");
    lame.set_sample_rate(TTSKoko::SAMPLE_RATE as u32).expect("Failed to set sample rate");
    lame.set_quality(3).expect("Failed to set quality"); // Quality range: 0 (best) to 9 (worst)
    lame.init_params().expect("Failed to initialize parameters");

    // Convert f32 samples to i16
    let pcm: Vec<i16> = raw_audio
        .iter()
        .map(|&x| (x * 32767.0) as i16)
        .collect();

    let mut mp3_data = Vec::new();
    let mut mp3_buffer = vec![0u8; pcm.len() * 2]; // Buffer size estimation

    let encoded = lame.encode(&pcm, &[], &mut mp3_buffer)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    mp3_data.extend_from_slice(&mp3_buffer[..encoded]);

    // Flush the encoder using our custom helper
    let mut flush_buffer = vec![0u8; 7200];
    let flush_len = flush_lame(&mut lame, &mut flush_buffer)?;
    mp3_data.extend_from_slice(&flush_buffer[..flush_len]);

    Ok(mp3_data)
}

fn flush_lame(lame: &mut Lame, flush_buffer: &mut [u8]) -> Result<usize, StatusCode> {
    // Retrieve the raw pointer from the Lame instance.
    // Assuming that Lame is a newtype wrapper around a pointer, we cast its address
    // to get the inner pointer (which is assumed to be at the beginning of the struct).
    let lame_ptr = unsafe {
        let ptr_ptr: *const *mut lame_t = lame as *const _ as *const *mut lame_t;
        *ptr_ptr
    };

    let flush_len = unsafe {
        lame_encode_flush(lame_ptr, flush_buffer.as_mut_ptr(), flush_buffer.len() as i32)
    };

    if flush_len < 0 {
        Err(StatusCode::INTERNAL_SERVER_ERROR)
    } else {
        Ok(flush_len as usize)
    }
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
