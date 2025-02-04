use crate::tts::koko::TTSKoko;
use crate::utils::wav::{write_audio_chunk, WavHeader};
use axum::http::{StatusCode, header::CONTENT_TYPE};
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;
use lame::Lame;
use lazy_static::lazy_static;
use std::sync::Mutex;

// Global Mutex to ensure MP3 encoding is not executed concurrently.
lazy_static! {
    static ref MP3_ENCODER_LOCK: Mutex<()> = Mutex::new(());
}

/// Helper to return true by default.
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
    file_path: Option<String>, // Present when the audio is written to a file.
    audio: Option<String>,     // Can be used if you need to return base64 encoded audio.
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
struct LameGlobalFlags {
    _private: [u8; 0],
}

// Alias for the LAME handle.
type LameT = LameGlobalFlags;

extern "C" {
    // Declaration for the native function:
    // int lame_encode_flush(lame_t *gfp, unsigned char *mp3buf, int size);
    fn lame_encode_flush(lame: *mut LameT, mp3buf: *mut u8, size: i32) -> i32;
}

/// Custom flush helper using FFI.
///
/// This accesses (via an unsafe cast) the underlying raw pointer of the Lame instance,
/// then calls the FFI flush function.
fn flush_lame(lame: &mut Lame, flush_buffer: &mut [u8]) -> Result<usize, StatusCode> {
    let lame_ptr = unsafe {
        // Cast the Lame instance to a pointer to a pointer of LameT.
        let ptr_ptr: *const *mut LameT = lame as *const _ as *const *mut LameT;
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

/// Converts raw audio samples (f32) to MP3-encoded bytes.
/// For MP3 encoding, we initialize LAME with 2 channels—even though our audio is mono—and supply
/// identical PCM data for both left and right channels.
fn encode_to_mp3(raw_audio: &[f32]) -> Result<Vec<u8>, StatusCode> {
    // Lock to ensure this section is executed by only one thread at a time.
    let _lock = MP3_ENCODER_LOCK.lock().unwrap();

    let mut lame = Lame::new().expect("Failed to initialize LAME");
    // For MP3 encoding, we set channels to 2 so that we duplicate the mono samples.
    lame.set_channels(2).expect("Failed to set channels");
    lame.set_sample_rate(TTSKoko::SAMPLE_RATE as u32)
        .expect("Failed to set sample rate");
    lame.set_quality(3).expect("Failed to set quality"); // Quality: 0 (best) to 9 (worst)
    lame.init_params().expect("Failed to initialize parameters");

    // Convert f32 samples to i16.
    let pcm: Vec<i16> = raw_audio.iter().map(|&x| (x * 32767.0) as i16).collect();

    let mut mp3_data = Vec::new();
    let mut mp3_buffer = vec![0u8; pcm.len() * 2]; // Estimate a buffer size.

    // Encode the PCM data.
    let encoded = lame.encode(&pcm, &pcm, &mut mp3_buffer)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    mp3_data.extend_from_slice(&mp3_buffer[..encoded]);

    // Flush the encoder using our custom flush helper.
    let mut flush_buffer = vec![0u8; 7200];
    let flush_len = flush_lame(&mut lame, &mut flush_buffer)?;
    mp3_data.extend_from_slice(&flush_buffer[..flush_len]);

    Ok(mp3_data)
}

/// The handler now returns a response that is fully compatible with the OpenAI TTS API:
/// - When `return_audio` is true, it returns raw binary audio data with the appropriate
///   Content-Type header so that clients can directly save or stream the file (e.g. via a curl --output command).
/// - When false, it writes the audio to disk and returns a JSON response including the file path.
async fn handle_tts(
    State(tts): State<TTSKoko>,
    Json(payload): Json<TTSRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let voice = payload.voice.unwrap_or_else(|| "af_sky".to_string());

    // Generate raw audio samples from TTS.
    let raw_audio = tts
        .tts_raw_audio(&payload.input, "en-us", &voice)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if payload.return_audio {
        // Return raw binary audio data.
        let (audio_data, content_type) = match payload.response_format {
            AudioFormat::Mp3 => {
                let data = encode_to_mp3(&raw_audio)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                (data, "audio/mpeg")
            }
            AudioFormat::Wav => {
                let mut wav_data = Vec::new();
                let header = WavHeader::new(1, TTSKoko::SAMPLE_RATE, 32);
                header.write_header(&mut wav_data)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                write_audio_chunk(&mut wav_data, &raw_audio)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                (wav_data, "audio/wav")
            }
        };
        let mut response = Response::new(audio_data.into());
        response.headers_mut().insert(
            CONTENT_TYPE,
            content_type.parse().expect("valid MIME type"),
        );
        Ok(response)
    } else {
        // Write audio to file and return a JSON response.
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let output_path = match payload.response_format {
            AudioFormat::Mp3 => {
                let path = format!("tmp/output_{}.mp3", timestamp);
                let data = encode_to_mp3(&raw_audio)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                std::fs::write(&path, data)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                path
            }
            AudioFormat::Wav => {
                let path = format!("tmp/output_{}.wav", timestamp);
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
                path
            }
        };

        let json_response = TTSResponse {
            status: "success".to_string(),
            file_path: Some(output_path),
            audio: None,
        };
        Ok(Json(json_response).into_response())
    }
}
