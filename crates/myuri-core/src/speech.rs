//! Speech requests: text to speech and speech to text for the providers the
//! configuration names. Pure building and parsing; the network lives in the
//! application crate.

use serde_json::{Value, json};

use crate::config::SpeechApi;
use crate::image::{self, Upload};
use crate::llm::{HttpRequest, url_encode};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TtsOptions {
    pub voice: String,
    pub stability: Option<f32>,
    pub similarity_boost: Option<f32>,
    pub style: Option<f32>,
    pub speed: Option<f32>,
    pub language: String,
    pub output_format: String,
}

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
    Pcm {
        rate: u32,
        bytes: Vec<u8>,
    },
    Mp3(Vec<u8>),
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
    let (field, filename, bytes) = file;
    image::multipart(
        fields,
        &[Upload {
            field,
            filename,
            mime: "audio/wav",
            bytes,
        }],
    )
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

/// Text to speech with the provider's default options and a voice override.
pub fn tts(
    api: SpeechApi,
    base: &str,
    key: &str,
    model: &str,
    voice: &str,
    text: &str,
) -> HttpRequest {
    tts_with_options(
        api,
        base,
        key,
        model,
        text,
        &TtsOptions {
            voice: voice.into(),
            ..Default::default()
        },
    )
}

const ELEVENLABS_PCM: &str = "pcm_24000";

pub fn tts_with_options(
    api: SpeechApi,
    base: &str,
    key: &str,
    model: &str,
    text: &str,
    options: &TtsOptions,
) -> HttpRequest {
    let voice = options.voice.as_str();
    let language = explicit_language(&options.language);
    match api {
        SpeechApi::Elevenlabs => {
            let voice = if voice.is_empty() {
                "21m00Tcm4TlvDq8ikWAM"
            } else {
                voice
            };
            let format = elevenlabs_format(&options.output_format);
            let mut body = json!({ "text": text, "model_id": model });
            let settings: serde_json::Map<String, Value> = [
                ("stability", options.stability),
                ("similarity_boost", options.similarity_boost),
                ("style", options.style),
                ("speed", options.speed.map(|speed| speed.clamp(0.7, 1.2))),
            ]
            .into_iter()
            .filter_map(|(name, value)| Some((name.to_string(), json!(rounded(value?)))))
            .collect();
            if !settings.is_empty() {
                body["voice_settings"] = Value::Object(settings);
            }
            if let Some(language) = language {
                body["language_code"] = json!(language);
            }
            request(
                api,
                base,
                key,
                &format!(
                    "/text-to-speech/{}?output_format={}",
                    url_encode(voice),
                    url_encode(&format)
                ),
                body,
            )
        }
        // The official Soniox TTS endpoint uses a separate host. Proxies use
        // the configured base for both speech APIs.
        SpeechApi::Soniox => {
            let mut body = json!({
                "model": model,
                "language": language.unwrap_or_else(|| speech_language(text)),
                "voice": if voice.is_empty() { "default" } else { voice },
                "audio_format": "wav",
                "text": text,
            });
            if let Some(speed) = options.speed {
                body["speed"] = json!(rounded(speed.clamp(0.7, 1.3)));
            }
            request(
                api,
                match base.trim_end_matches('/') {
                    "https://api.soniox.com" | "https://api.soniox.com/v1" => {
                        "https://tts-rt.soniox.com"
                    }
                    _ => base,
                },
                key,
                "/tts",
                body,
            )
        }
        SpeechApi::Openai => {
            let mut body = json!({
                "model": model,
                "voice": if voice.is_empty() { "alloy" } else { voice },
                "input": text,
                "response_format": "wav",
            });
            if let Some(speed) = options.speed {
                body["speed"] = json!(rounded(speed));
            }
            request(api, base, key, "/audio/speech", body)
        }
    }
}

fn explicit_language(language: &str) -> Option<&str> {
    let language = language.trim();
    (!language.is_empty() && !language.eq_ignore_ascii_case("auto")).then_some(language)
}

fn elevenlabs_format(format: &str) -> String {
    let trimmed = format.trim();
    if trimmed.is_empty() {
        ELEVENLABS_PCM.to_string()
    } else {
        trimmed.to_ascii_lowercase()
    }
}

/// Normalize and validate an ElevenLabs `output_format` before sending a request.
pub fn validate_elevenlabs_format(format: &str) -> Result<String, String> {
    let normalized = elevenlabs_format(format);
    match normalized.split('_').next() {
        Some("wav" | "mp3") => Ok(normalized),
        Some("pcm") => {
            normalized
                .strip_prefix("pcm_")
                .and_then(|rate| rate.parse::<u32>().ok())
                .filter(|&rate| rate > 0 && rate <= u32::MAX / 2)
                .ok_or_else(|| "PCM 输出格式缺少有效采样率".to_string())?;
            Ok(normalized)
        }
        _ => Err("语音输出格式只支持 PCM、WAV 和 MP3".into()),
    }
}

fn rounded(value: f32) -> f64 {
    (f64::from(value) * 1000.0).round() / 1000.0
}

/// The response body in the format requested from the speech provider.
pub fn tts_audio(api: SpeechApi, body: &[u8], output_format: &str) -> Result<Audio, String> {
    if api != SpeechApi::Elevenlabs || body.starts_with(b"RIFF") {
        return Ok(Audio::Wav(body.to_vec()));
    }
    let format = validate_elevenlabs_format(output_format)?;
    match format.split('_').next() {
        Some("wav") => Ok(Audio::Wav(body.to_vec())),
        Some("mp3") => Ok(Audio::Mp3(body.to_vec())),
        Some("pcm") => {
            let rate = format
                .strip_prefix("pcm_")
                .and_then(|rate| rate.parse().ok())
                .unwrap();
            Ok(Audio::Pcm {
                rate,
                bytes: body.to_vec(),
            })
        }
        _ => unreachable!(),
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
                name: voice
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

/// A GET request to list the speech models if the API exposes a catalog.
pub fn models_request(api: SpeechApi, base: &str, key: &str) -> Option<HttpRequest> {
    match api {
        SpeechApi::Elevenlabs | SpeechApi::Openai => {
            Some(request(api, base, key, "/models", Value::Null))
        }
        SpeechApi::Soniox => None,
    }
}

/// The ASR and TTS models in a provider's catalog, in upstream order.
pub fn parse_models(api: SpeechApi, body: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let response: Value =
        serde_json::from_str(body).map_err(|_| "上游的回答不是 JSON".to_string())?;
    let mut asr = Vec::new();
    let mut tts = Vec::new();
    match api {
        SpeechApi::Elevenlabs => {
            let models = response.as_array().ok_or("上游的回答里没有模型列表")?;
            for model in models {
                let Some(id) = model.get("model_id").and_then(Value::as_str) else {
                    continue;
                };
                if model["can_do_text_to_speech"] == true
                    && !tts.iter().any(|existing| existing == id)
                {
                    tts.push(id.to_string());
                }
            }
        }
        SpeechApi::Openai => {
            let models = response
                .get("data")
                .and_then(Value::as_array)
                .ok_or("上游的回答里没有模型列表")?;
            for model in models {
                let Some(id) = model.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let name = id.to_ascii_lowercase();
                let list = if name.contains("whisper") || name.contains("transcribe") {
                    &mut asr
                } else if name.contains("tts") || name.contains("speech") {
                    &mut tts
                } else {
                    continue;
                };
                if !list.iter().any(|existing| existing == id) {
                    list.push(id.to_string());
                }
            }
        }
        SpeechApi::Soniox => return Err("这个语音接口没有模型列表".into()),
    }
    Ok((asr, tts))
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
            request.headers[1] = (
                "content-type".into(),
                format!("multipart/form-data; boundary={boundary}"),
            );
            (request, body, String::new())
        }
        // Soniox transcribes asynchronously: upload the file first.
        SpeechApi::Soniox => {
            let (body, boundary) = multipart(&[], ("file", "audio.wav", audio));
            let mut request = request(api, base, key, "/files", Value::Null);
            request.headers[1] = (
                "content-type".into(),
                format!("multipart/form-data; boundary={boundary}"),
            );
            (request, body, String::new())
        }
        SpeechApi::Openai => {
            let (body, boundary) = multipart(
                &[("model", model), ("response_format", "json")],
                ("file", "audio.wav", audio),
            );
            let mut request = request(api, base, key, "/audio/transcriptions", Value::Null);
            request.headers[1] = (
                "content-type".into(),
                format!("multipart/form-data; boundary={boundary}"),
            );
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
    request(
        SpeechApi::Soniox,
        base,
        key,
        &format!("/transcriptions/{id}"),
        Value::Null,
    )
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
    let mut request = request(
        SpeechApi::Soniox,
        base,
        key,
        &format!("/{kind}/{id}"),
        Value::Null,
    );
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

fn speech_language(text: &str) -> &'static str {
    let mut ideographs = false;
    for c in text.chars() {
        match c as u32 {
            0x3041..=0x3096
            | 0x30a1..=0x30fa
            | 0x31f0..=0x31ff
            | 0xff66..=0xff9d
            | 0x1b000..=0x1b16f => return "ja",
            0x1100..=0x11ff | 0x3131..=0x318e | 0xa960..=0xa97c | 0xac00..=0xd7fb => return "ko",
            0x3007
            | 0x3400..=0x4dbf
            | 0x4e00..=0x9fff
            | 0xf900..=0xfaff
            | 0x20000..=0x2ee5f
            | 0x2f800..=0x2fa1f
            | 0x30000..=0x3347f => ideographs = true,
            _ => {}
        }
    }
    if ideographs { "zh" } else { "en" }
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
        let eleven = tts(
            SpeechApi::Elevenlabs,
            "https://api.elevenlabs.io/v1",
            "k",
            "eleven_v4",
            "v1",
            "你好",
        );
        assert!(
            eleven
                .url
                .ends_with("/text-to-speech/v1?output_format=pcm_24000")
        );
        assert!(
            eleven
                .headers
                .iter()
                .any(|(n, v)| n == "xi-api-key" && v == "k")
        );
        assert_eq!(eleven.body["model_id"], "eleven_v4");
        assert_eq!(eleven.body["text"], "你好");

        let soniox = tts(
            SpeechApi::Soniox,
            "https://api.soniox.com/v1",
            "k",
            "tts-v6",
            "",
            "hello",
        );
        assert_eq!(soniox.url, "https://tts-rt.soniox.com/tts");
        assert_eq!(soniox.body["language"], "en");
        let soniox_zh = tts(
            SpeechApi::Soniox,
            "https://api.soniox.com/v1",
            "k",
            "tts-v6",
            "",
            "你好",
        );
        assert_eq!(soniox_zh.body["language"], "zh");

        let audio = tts_audio(SpeechApi::Elevenlabs, b"pcm", "").unwrap();
        assert_eq!(
            audio,
            Audio::Pcm {
                rate: 24000,
                bytes: b"pcm".to_vec()
            }
        );
    }

    #[test]
    fn tts_options_and_selected_audio_format() {
        let base = "https://speech.invalid/v1";
        let mut options = TtsOptions {
            voice: "voice/id?".into(),
            stability: Some(0.3),
            similarity_boost: Some(0.76),
            speed: Some(1.1549),
            language: "ja".into(),
            output_format: "mp3_44100_128".into(),
            ..Default::default()
        };
        let eleven = tts_with_options(
            SpeechApi::Elevenlabs,
            base,
            "fixture-key",
            "model",
            "試聴",
            &options,
        );
        assert_eq!(
            eleven.url,
            format!("{base}/text-to-speech/voice%2Fid%3F?output_format=mp3_44100_128")
        );
        assert_eq!(
            eleven.body["voice_settings"],
            json!({ "stability": 0.3, "similarity_boost": 0.76, "speed": 1.155 })
        );
        assert_eq!(eleven.body["language_code"], "ja");
        assert_eq!(
            tts_audio(SpeechApi::Elevenlabs, b"mp3", &options.output_format),
            Ok(Audio::Mp3(b"mp3".to_vec()))
        );
        assert_eq!(
            tts_audio(SpeechApi::Elevenlabs, b"pcm", "pcm_44100"),
            Ok(Audio::Pcm {
                rate: 44100,
                bytes: b"pcm".to_vec()
            })
        );
        assert!(tts_audio(SpeechApi::Elevenlabs, b"pcm", "pcm").is_err());
        assert_eq!(
            tts_audio(SpeechApi::Elevenlabs, b"wav", "wav_44100"),
            Ok(Audio::Wav(b"wav".to_vec()))
        );

        let openai = tts_with_options(
            SpeechApi::Openai,
            base,
            "fixture-key",
            "model",
            "sample",
            &options,
        );
        assert_eq!(openai.body["speed"], 1.155);
        let soniox = tts_with_options(
            SpeechApi::Soniox,
            base,
            "fixture-key",
            "model",
            "sample",
            &options,
        );
        assert_eq!(soniox.url, format!("{base}/tts"));
        assert_eq!(soniox.body["language"], "ja");
        assert_eq!(soniox.body["speed"], 1.155);
        options.speed = Some(1.5);
        let eleven_clamped = tts_with_options(
            SpeechApi::Elevenlabs,
            base,
            "fixture-key",
            "model",
            "fast",
            &options,
        );
        assert_eq!(eleven_clamped.body["voice_settings"]["speed"], 1.2);
        let soniox_clamped = tts_with_options(
            SpeechApi::Soniox,
            base,
            "fixture-key",
            "model",
            "fast",
            &options,
        );
        assert_eq!(soniox_clamped.body["speed"], 1.3);
        assert_eq!(validate_elevenlabs_format("  MP3_44100_128 ").unwrap(), "mp3_44100_128");
        assert!(validate_elevenlabs_format("opus_48000_128").is_err());
        options.language = "auto".into();
        let soniox = tts_with_options(
            SpeechApi::Soniox,
            base,
            "fixture-key",
            "model",
            "字 \u{20000}",
            &options,
        );
        assert_eq!(soniox.body["language"], "zh");
        for (text, language) in [
            ("\u{20000}", "zh"),
            ("こんにちは", "ja"),
            ("안녕하세요", "ko"),
        ] {
            assert_eq!(speech_language(text), language);
        }
    }

    #[test]
    fn speech_model_lists_are_filtered() {
        let base = "https://speech.invalid/v1";
        let req = models_request(SpeechApi::Elevenlabs, base, "fixture-key").unwrap();
        assert_eq!(req.url, format!("{base}/models"));
        assert!(models_request(SpeechApi::Soniox, base, "fixture-key").is_none());
        let eleven = r#"[{"model_id":"talk","can_do_text_to_speech":true},{"model_id":"other","can_do_text_to_speech":false}]"#;
        assert_eq!(
            parse_models(SpeechApi::Elevenlabs, eleven),
            Ok((vec![], vec!["talk".into()]))
        );
        let openai = r#"{"data":[{"id":"tts-1"},{"id":"gpt-speech"},{"id":"whisper-1"},{"id":"gpt-transcribe"},{"id":"other"}]}"#;
        assert_eq!(
            parse_models(SpeechApi::Openai, openai),
            Ok((
                vec!["whisper-1".into(), "gpt-transcribe".into()],
                vec!["tts-1".into(), "gpt-speech".into()]
            ))
        );
        assert!(parse_models(SpeechApi::Openai, "{}").is_err());
    }

    #[test]
    fn stt_and_soniox_flow() {
        let (req, body, _) = stt(SpeechApi::Elevenlabs, "https://b", "k", "scribe_v2", b"wav");
        assert!(req.url.ends_with("/speech-to-text"));
        assert!(
            req.headers[1]
                .1
                .starts_with("multipart/form-data; boundary=")
        );
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"model_id\"") && text.contains("scribe_v2"));

        let (req, body, _) = stt(
            SpeechApi::Soniox,
            "https://api.soniox.com/v1",
            "k",
            "stt",
            b"wav",
        );
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
        let voices = read_voices(
            "{\"voices\":[{\"voice_id\":\"v1\",\"name\":\"Alice\"},{\"voice_id\":\"v2\"}]}",
        );
        assert_eq!(voices.len(), 2);
        assert_eq!(voices[0].name, "Alice");
        assert!(voices[1].name.is_empty());
    }
}
