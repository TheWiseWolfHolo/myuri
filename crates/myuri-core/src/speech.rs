//! Speech requests: text to speech and speech to text for the providers the
//! configuration names. Pure building and parsing; the network lives in the
//! application crate.

use serde_json::{Value, json};

use crate::config::SpeechApi;
use crate::llm::HttpRequest;

/// A voice listing entry: what the API calls it and what we send it as.
#[derive(Clone, Debug, PartialEq)]
pub struct Voice {
    pub id: String,
    pub name: String,
}

/// The audio the services answer with.
#[derive(Clone, Debug, PartialEq)]
pub enum Audio {
    /// A complete WAV file already.
    Wav(Vec<u8>),
    /// Signed 16-bit little-endian mono PCM at `rate` Hz; wrap before playing.
    Pcm { rate: u32, bytes: Vec<u8> },
}

/// Wrap raw s16le mono PCM in a minimal WAV header.
pub fn wav(rate: u32, pcm: &[u8]) -> Vec<u8> {
    let data = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM header size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

/// A multipart/form-data body of simple text fields and one file.
pub fn multipart(fields: &[(&str, &str)], file: (&str, &str, &[u8])) -> (Vec<u8>, String) {
    let hash = file.2.iter().fold(0xcbf29ce484222325u64, |h, b| h.wrapping_mul(0x100000001b3) ^ *b as u64);
    let boundary = format!("myuri-{hash:016x}");
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    let (field, filename, bytes) = file;
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (body, boundary)
}

fn request(api: SpeechApi, base: &str, key: &str, path: &str, body: Value) -> HttpRequest {
    let base = base.trim_end_matches('/');
    let auth = match api {
        SpeechApi::Elevenlabs => ("xi-api-key".to_string(), key.to_string()),
        _ => ("authorization".to_string(), format!("Bearer {key}")),
    };
    HttpRequest {
        url: format!("{base}{path}"),
        headers: vec![auth, ("content-type".into(), "application/json".into())],
        body,
    }
}

/// Text to speech. ElevenLabs streams raw PCM at 24 kHz; Soniox answers a WAV.
pub fn tts(api: SpeechApi, base: &str, key: &str, model: &str, voice: &str, text: &str) -> HttpRequest {
    match api {
        SpeechApi::Elevenlabs => {
            let voice = if voice.is_empty() { "21m00Tcm4TlvDq8ikWAM" } else { voice };
            request(
                api,
                base,
                key,
                &format!("/text-to-speech/{voice}?output_format=pcm_24000"),
                json!({ "text": text, "model_id": model }),
            )
        }
        // Soniox's TTS lives on its own host, not under the API base.
        SpeechApi::Soniox => request(
            api,
            "https://tts-rt.soniox.com",
            key,
            "/tts",
            json!({
                "model": model,
                "language": if has_cjk(text) { "zh" } else { "en" },
                "voice": if voice.is_empty() { "default" } else { voice },
                "audio_format": "wav",
                "text": text,
            }),
        ),
        SpeechApi::Openai => request(
            api,
            base,
            key,
            "/audio/speech",
            json!({ "model": model, "voice": if voice.is_empty() { "alloy" } else { voice }, "input": text, "response_format": "wav" }),
        ),
    }
}

/// What a TTS response body contains.
pub fn tts_audio(api: SpeechApi, body: &[u8]) -> Audio {
    match api {
        SpeechApi::Elevenlabs => Audio::Pcm { rate: 24000, bytes: body.to_vec() },
        _ => Audio::Wav(body.to_vec()),
    }
}

/// A request listing the voices a provider offers, if it has such an endpoint.
pub fn voices(api: SpeechApi, base: &str, key: &str) -> Option<HttpRequest> {
    match api {
        SpeechApi::Elevenlabs => Some(request(api, base, key, "/voices", Value::Null)),
        _ => None,
    }
}

/// The voice_id/name pairs out of ElevenLabs' `GET /voices` reply.
pub fn read_voices(body: &str) -> Vec<Voice> {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|json| json.get("voices")?.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|voice| {
            Some(Voice {
                id: voice.get("voice_id")?.as_str()?.to_string(),
                name: voice.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// Speech to text: the first step of the call, or the whole call.
/// Returns (request, multipart body, content type) — the request body field
/// is unused for multipart posts.
pub fn stt(
    api: SpeechApi,
    base: &str,
    key: &str,
    model: &str,
    audio: &[u8],
) -> (HttpRequest, Vec<u8>, String) {
    match api {
        SpeechApi::Elevenlabs => {
            let (body, boundary) = multipart(&[("model_id", model)], ("file", "audio.wav", audio));
            let mut request = request(api, base, key, "/speech-to-text", Value::Null);
            request.headers[1] = ("content-type".into(), format!("multipart/form-data; boundary={boundary}"));
            (request, body, String::new())
        }
        // Soniox transcribes asynchronously: upload the file first.
        SpeechApi::Soniox => {
            let (body, boundary) = multipart(&[], ("file", "audio.wav", audio));
            let mut request = request(api, base, key, "/files", Value::Null);
            request.headers[1] = ("content-type".into(), format!("multipart/form-data; boundary={boundary}"));
            (request, body, String::new())
        }
        SpeechApi::Openai => {
            let (body, boundary) =
                multipart(&[("model", model), ("response_format", "json")], ("file", "audio.wav", audio));
            let mut request = request(api, base, key, "/audio/transcriptions", Value::Null);
            request.headers[1] = ("content-type".into(), format!("multipart/form-data; boundary={boundary}"));
            (request, body, String::new())
        }
    }
}

/// The file id Soniox answers `POST /files` with.
pub fn read_file_id(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|json| json.get("id")?.as_str().map(str::to_string))
}

/// Ask Soniox to start transcribing the uploaded file.
pub fn soniox_transcribe(base: &str, key: &str, model: &str, file_id: &str) -> HttpRequest {
    request(
        SpeechApi::Soniox,
        base,
        key,
        "/transcriptions",
        json!({ "model": model, "file_id": file_id }),
    )
}

/// Poll a Soniox transcription: GET /transcriptions/{id}.
pub fn soniox_poll(base: &str, key: &str, id: &str) -> HttpRequest {
    request(SpeechApi::Soniox, base, key, &format!("/transcriptions/{id}"), Value::Null)
}

/// Fetch a finished Soniox transcript: GET /transcriptions/{id}/transcript.
pub fn soniox_transcript(base: &str, key: &str, id: &str) -> HttpRequest {
    request(
        SpeechApi::Soniox,
        base,
        key,
        &format!("/transcriptions/{id}/transcript"),
        Value::Null,
    )
}

/// A Soniox transcription's status, and whether it is done.
pub fn read_status(body: &str) -> Result<bool, String> {
    let json: Value = serde_json::from_str(body).map_err(|_| "上游的回答不是 JSON".to_string())?;
    match json.get("status").and_then(Value::as_str) {
        Some("completed") => Ok(true),
        Some("error") => {
            let reason = json
                .get("error_message")
                .and_then(Value::as_str)
                .unwrap_or("上游转写失败");
            Err(reason.to_string())
        }
        _ => Ok(false),
    }
}

/// Delete a Soniox resource (file or transcription). Best effort.
pub fn soniox_delete(base: &str, key: &str, kind: &str, id: &str) -> HttpRequest {
    let mut request = request(SpeechApi::Soniox, base, key, &format!("/{kind}/{id}"), Value::Null);
    // The application swaps POST for DELETE; this flag marks it.
    request.body = json!({ "_method": "DELETE" });
    request
}

/// The transcribed text out of a reply: `text` or nested under `transcript`.
pub fn read_text(body: &str) -> Result<String, String> {
    let json: Value = serde_json::from_str(body).map_err(|_| "上游的回答不是 JSON".to_string())?;
    json.get("text")
        .or_else(|| json.get("transcript"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "上游的转写结果里没有文字".to_string())
}

/// Whether the text has a CJK character (picks the speech language).
fn has_cjk(text: &str) -> bool {
    text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_wraps_pcm() {
        let pcm = vec![1u8, 2, 3, 4];
        let file = wav(24000, &pcm);
        assert_eq!(&file[..4], b"RIFF");
        assert_eq!(&file[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes(file[24..28].try_into().unwrap()), 24000);
        assert_eq!(u32::from_le_bytes(file[40..44].try_into().unwrap()), 4);
        assert_eq!(&file[44..], [1, 2, 3, 4]);
        assert_eq!(u32::from_le_bytes(file[4..8].try_into().unwrap()), 40);
    }

    #[test]
    fn tts_requests_are_built() {
        let eleven = tts(SpeechApi::Elevenlabs, "https://api.elevenlabs.io/v1", "k", "eleven_v4", "v1", "你好");
        assert!(eleven.url.ends_with("/text-to-speech/v1?output_format=pcm_24000"));
        assert!(eleven.headers.iter().any(|(n, v)| n == "xi-api-key" && v == "k"));
        assert_eq!(eleven.body["model_id"], "eleven_v4");
        assert_eq!(eleven.body["text"], "你好");

        let soniox = tts(SpeechApi::Soniox, "https://api.soniox.com/v1", "k", "tts-v6", "", "hello");
        assert_eq!(soniox.url, "https://tts-rt.soniox.com/tts");
        assert_eq!(soniox.body["language"], "en");
        let soniox_zh = tts(SpeechApi::Soniox, "https://api.soniox.com/v1", "k", "tts-v6", "", "你好");
        assert_eq!(soniox_zh.body["language"], "zh");

        let audio = tts_audio(SpeechApi::Elevenlabs, b"pcm");
        assert_eq!(audio, Audio::Pcm { rate: 24000, bytes: b"pcm".to_vec() });
    }

    #[test]
    fn stt_and_soniox_flow() {
        let (req, body, _) = stt(SpeechApi::Elevenlabs, "https://b", "k", "scribe_v2", b"wav");
        assert!(req.url.ends_with("/speech-to-text"));
        assert!(req.headers[1].1.starts_with("multipart/form-data; boundary="));
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"model_id\"") && text.contains("scribe_v2"));

        let (req, body, _) = stt(SpeechApi::Soniox, "https://api.soniox.com/v1", "k", "stt", b"wav");
        assert_eq!(req.url, "https://api.soniox.com/v1/files");
        assert!(String::from_utf8_lossy(&body).contains("name=\"file\""));

        assert_eq!(read_file_id("{\"id\":\"f1\"}"), Some("f1".into()));
        let start = soniox_transcribe("https://api.soniox.com/v1", "k", "stt", "f1");
        assert_eq!(start.body["file_id"], "f1");
        assert_eq!(read_status("{\"status\":\"completed\"}"), Ok(true));
        assert_eq!(read_status("{\"status\":\"transcribing\"}"), Ok(false));
        assert!(read_status("{\"status\":\"error\",\"error_message\":\"bad\"}").is_err());
        assert_eq!(read_text("{\"text\":\"你好\"}").unwrap(), "你好");
    }

    #[test]
    fn voices_are_read() {
        let voices = read_voices("{\"voices\":[{\"voice_id\":\"v1\",\"name\":\"Alice\"},{\"voice_id\":\"v2\"}]}");
        assert_eq!(voices.len(), 2);
        assert_eq!(voices[0].name, "Alice");
        assert!(voices[1].name.is_empty());
    }
}
