//! Speech: text to speech and speech to text through the providers named in
//! `[speech]`, playback through PlaySoundW and recording through waveIn.
//! Everything that talks to a service blocks; call it off the UI thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
    CALLBACK_NULL, HWAVEIN, PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT, SND_PURGE,
    WAVE_FORMAT_PCM, WAVE_MAPPER, WAVEHDR, WAVEFORMATEX, WHDR_DONE, waveInAddBuffer,
    waveInClose, waveInOpen, waveInPrepareHeader, waveInReset, waveInStart, waveInStop,
    waveInUnprepareHeader,
};
use windows::core::PCWSTR;

use myuri_core::config::{Provider, SpeechApi};
use myuri_core::llm;
use myuri_core::speech::{self, Audio, Voice};

use crate::{net, secrets};

/// How long a Soniox transcription may take before giving up.
const PATIENCE: Duration = Duration::from_secs(90);

/// Speak text; returns a WAV file's bytes.
pub fn tts(provider: &Provider, model: &str, text: &str) -> Result<Vec<u8>, String> {
    let Some(api) = provider.speech_api else {
        return Err(format!("\"{}\" 不支持语音", provider.name));
    };
    let stored = secrets::read(&provider.id)
        .ok_or_else(|| format!("供应商 \"{0}\" 还没有密钥，用 myuri key {0} 设置", provider.id))?;
    let mut failure = "密钥都停用了".to_string();
    for key in provider.keys_to_try(&stored, 0) {
        match tts_once(api, provider, key, model, text) {
            Ok(wav) => return Ok(wav),
            Err((reason, try_next)) => {
                failure = reason;
                if !try_next {
                    break;
                }
            }
        }
    }
    Err(failure)
}

fn tts_once(
    api: SpeechApi,
    provider: &Provider,
    key: &str,
    model: &str,
    text: &str,
) -> Result<Vec<u8>, (String, bool)> {
    let mut sent = speech::tts(api, &provider.base_url, key, model, &provider.voice, text);
    sent.headers.extend(provider.headers.iter().map(|(n, v)| (n.clone(), v.clone())));
    let (status, body) = net::post(&sent.url, &sent.headers, sent.body.to_string().as_bytes())
        .map_err(|reason| (reason, true))?;
    if !(200..300).contains(&status) {
        return Err((
            llm::describe_failure(status, &String::from_utf8_lossy(&body)),
            llm::another_key_may_work(status),
        ));
    }
    Ok(match speech::tts_audio(api, &body) {
        Audio::Wav(wav) => wav,
        Audio::Pcm { rate, bytes } => speech::wav(rate, &bytes),
    })
}

/// Transcribe a WAV file's bytes.
pub fn stt(provider: &Provider, model: &str, audio: &[u8]) -> Result<String, String> {
    let Some(api) = provider.speech_api else {
        return Err(format!("\"{}\" 不支持语音", provider.name));
    };
    let stored = secrets::read(&provider.id)
        .ok_or_else(|| format!("供应商 \"{0}\" 还没有密钥", provider.id))?;
    let mut failure = "密钥都停用了".to_string();
    for key in provider.keys_to_try(&stored, 0) {
        let result = if api == SpeechApi::Soniox {
            soniox_stt(provider, key, model, audio)
        } else {
            stt_once(api, provider, key, model, audio)
        };
        match result {
            Ok(text) => return Ok(text),
            Err((reason, try_next)) => {
                failure = reason;
                if !try_next {
                    break;
                }
            }
        }
    }
    Err(failure)
}

/// One multipart post carrying the audio, like ElevenLabs' /speech-to-text.
fn stt_once(
    api: SpeechApi,
    provider: &Provider,
    key: &str,
    model: &str,
    audio: &[u8],
) -> Result<String, (String, bool)> {
    let (mut sent, body, _) = speech::stt(api, &provider.base_url, key, model, audio);
    sent.headers.extend(provider.headers.iter().map(|(n, v)| (n.clone(), v.clone())));
    let (status, reply) = net::post(&sent.url, &sent.headers, &body).map_err(|reason| (reason, true))?;
    if !(200..300).contains(&status) {
        return Err((
            llm::describe_failure(status, &String::from_utf8_lossy(&reply)),
            llm::another_key_may_work(status),
        ));
    }
    speech::read_text(&String::from_utf8_lossy(&reply)).map_err(|reason| (reason, false))
}

/// Soniox transcribes asynchronously: upload, start, poll, fetch, clean up.
fn soniox_stt(
    provider: &Provider,
    key: &str,
    model: &str,
    audio: &[u8],
) -> Result<String, (String, bool)> {
    let base = provider.base_url.trim_end_matches('/');
    let fatal = |reason: String| (reason, false);
    let post = |request: &myuri_core::llm::HttpRequest, body: &[u8]| {
        let mut headers = request.headers.clone();
        headers.extend(provider.headers.iter().map(|(n, v)| (n.clone(), v.clone())));
        let (status, reply) = net::post(&request.url, &headers, body).map_err(|r| (r, true))?;
        if !(200..300).contains(&status) {
            return Err((
                llm::describe_failure(status, &String::from_utf8_lossy(&reply)),
                llm::another_key_may_work(status),
            ));
        }
        Ok(String::from_utf8_lossy(&reply).into_owned())
    };
    let get = |request: &myuri_core::llm::HttpRequest| {
        net::get(&request.url, &request.headers)
            .map(|body| String::from_utf8_lossy(&body).into_owned())
            .map_err(|reason| (reason, true))
    };
    // 1. Upload the recording.
    let (request, body, _) = speech::stt(SpeechApi::Soniox, base, key, model, audio);
    let file_id = speech::read_file_id(&post(&request, &body)?).ok_or_else(|| fatal("上传录音没有拿到文件号".into()))?;
    // 2. Start the transcription.
    let request = speech::soniox_transcribe(base, key, model, &file_id);
    let id = speech::read_file_id(&post(&request, request.body.to_string().as_bytes())?)
        .ok_or_else(|| fatal("转写任务没有拿到编号".into()))?;
    // 3. Poll, then fetch the transcript.
    let cleanup = |provider: &Provider, key: &str, id: &str, file_id: &str| {
        let request = speech::soniox_delete(&provider.base_url, key, "transcriptions", id);
        let _ = net::delete(&request.url, &request.headers);
        let request = speech::soniox_delete(&provider.base_url, key, "files", file_id);
        let _ = net::delete(&request.url, &request.headers);
    };
    let started = Instant::now();
    let text = loop {
        if started.elapsed() > PATIENCE {
            cleanup(provider, key, &id, &file_id);
            return Err(fatal("转写等太久没出结果".into()));
        }
        match speech::read_status(&get(&speech::soniox_poll(base, key, &id))?) {
            Ok(true) => break speech::read_text(&get(&speech::soniox_transcript(base, key, &id))?)
                .map_err(fatal),
            Ok(false) => std::thread::sleep(Duration::from_millis(800)),
            Err(reason) => {
                cleanup(provider, key, &id, &file_id);
                return Err((reason, false));
            }
        }
    };
    cleanup(provider, key, &id, &file_id);
    text
}

/// The voices a provider offers; only ElevenLabs lists them.
pub fn voices(provider: &Provider) -> Result<Vec<Voice>, String> {
    let Some(api) = provider.speech_api else { return Ok(Vec::new()) };
    let stored = secrets::read(&provider.id)
        .ok_or_else(|| format!("供应商 \"{0}\" 还没有密钥", provider.id))?;
    let mut failure = "密钥都停用了".to_string();
    for key in provider.keys_to_try(&stored, 0) {
        let Some(request) = speech::voices(api, &provider.base_url, key) else { break };
        let mut sent = request;
        sent.headers.extend(provider.headers.iter().map(|(n, v)| (n.clone(), v.clone())));
        match net::get(&sent.url, &sent.headers) {
            Ok(body) => return Ok(speech::read_voices(&String::from_utf8_lossy(&body))),
            Err(reason) => failure = reason,
        }
    }
    Err(failure)
}

/// The WAV file whose bytes are playing right now; kept alive while it plays.
fn playing() -> &'static Mutex<Vec<u8>> {
    static PLAYING: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();
    PLAYING.get_or_init(|| Mutex::new(Vec::new()))
}

/// Play WAV bytes from memory. Another play or `silence` cuts it short.
pub fn play(bytes: Vec<u8>) {
    let mut slot = playing().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = bytes;
    unsafe {
        let _ = PlaySoundW(
            PCWSTR(slot.as_ptr() as *const u16),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
    }
}

/// Stop whatever is playing.
pub fn silence() {
    unsafe {
        let _ = PlaySoundW(None, None, SND_PURGE);
    }
    if let Ok(mut slot) = playing().lock() {
        slot.clear();
    }
}

/// 16 kHz mono s16le — what speech models ask for.
const RATE: u32 = 16000;
const BUFFER: usize = (RATE as usize) * 2 / 10;

/// An open microphone recording; `stop` returns a WAV file's bytes.
pub struct Recorder {
    device: HWAVEIN,
    /// Memory the prepared headers point into; only kept alive here.
    _buffers: Vec<Box<[u8]>>,
    headers: Vec<Box<WAVEHDR>>,
    recorded: Arc<Mutex<Vec<u8>>>,
    halt: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    /// When it started, for the elapsed read-out.
    pub started: Instant,
}

/// Start recording. The poller thread collects finished buffers.
pub fn record() -> Result<Recorder, String> {
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: 1,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * 2,
        nBlockAlign: 2,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    unsafe {
        let mut device = HWAVEIN::default();
        if waveInOpen(Some(&mut device), WAVE_MAPPER, &format, 0, 0, CALLBACK_NULL) != 0 {
            return Err("打不开麦克风".into());
        }
        let mut buffers = Vec::new();
        let mut headers: Vec<Box<WAVEHDR>> = Vec::new();
        for _ in 0..4 {
            let mut buffer = vec![0u8; BUFFER].into_boxed_slice();
            let header = Box::new(WAVEHDR {
                lpData: windows::core::PSTR(buffer.as_mut_ptr()),
                dwBufferLength: BUFFER as u32,
                ..Default::default()
            });
            buffers.push(buffer);
            headers.push(header);
        }
        let fail = |message: &str| -> String {
            waveInClose(device);
            message.to_string()
        };
        for header in headers.iter_mut() {
            if waveInPrepareHeader(device, &mut **header, size_of::<WAVEHDR>() as u32) != 0 {
                return Err(fail("麦克风准备录音缓冲失败"));
            }
            if waveInAddBuffer(device, &mut **header, size_of::<WAVEHDR>() as u32) != 0 {
                return Err(fail("麦克风加录音缓冲失败"));
            }
        }
        if waveInStart(device) != 0 {
            return Err(fail("麦克风开录失败"));
        }
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let halt = Arc::new(AtomicBool::new(false));
        let worker = {
            let device = device.0 as usize;
            let header_ptrs: Vec<usize> =
                headers.iter().map(|header| &**header as *const WAVEHDR as usize).collect();
            let recorded = recorded.clone();
            let halt = halt.clone();
            std::thread::spawn(move || {
                while !halt.load(Ordering::Relaxed) {
                    for ptr in &header_ptrs {
                        let header = &mut *(*ptr as *mut WAVEHDR);
                        if header.dwFlags & WHDR_DONE == 0 {
                            continue;
                        }
                        let data = std::slice::from_raw_parts(
                            header.lpData.0,
                            (header.dwBytesRecorded as usize).min(BUFFER),
                        );
                        recorded
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .extend_from_slice(data);
                        // Hand the buffer back to the driver.
                        waveInUnprepareHeader(
                            HWAVEIN(device as _),
                            header,
                            size_of::<WAVEHDR>() as u32,
                        );
                        header.dwBytesRecorded = 0;
                        header.dwFlags = 0;
                        waveInPrepareHeader(HWAVEIN(device as _), header, size_of::<WAVEHDR>() as u32);
                        waveInAddBuffer(HWAVEIN(device as _), header, size_of::<WAVEHDR>() as u32);
                    }
                    std::thread::sleep(Duration::from_millis(12));
                }
            })
        };
        Ok(Recorder {
            device,
            _buffers: buffers,
            headers,
            recorded,
            halt,
            worker: Some(worker),
            started: Instant::now(),
        })
    }
}

impl Recorder {
    /// Stop recording and wrap what was captured as a WAV file.
    pub fn stop(mut self) -> Vec<u8> {
        self.finish();
        let pcm = std::mem::take(
            &mut *self.recorded.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        speech::wav(RATE, &pcm)
    }

    fn finish(&mut self) {
        self.halt.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        unsafe {
            waveInStop(self.device);
            waveInReset(self.device);
            for header in self.headers.iter_mut() {
                waveInUnprepareHeader(self.device, &mut **header, size_of::<WAVEHDR>() as u32);
            }
            waveInClose(self.device);
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The WAV plumbing a Recorder relies on, without touching hardware.
    #[test]
    fn wav_wraps_pcm() {
        let wav = speech::wav(16000, &[1, 0, 255, 255]);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[24..28], &(16000u32).to_le_bytes());
        assert_eq!(&wav[44..], &[1, 0, 255, 255]);
    }

    /// Live round trip against the configured accounts: synthesize a sentence
    /// with each speech provider, save the WAV under target/, then transcribe
    /// one of them back. Run with `cargo test -- --ignored live`.
    #[test]
    #[ignore]
    fn live() {
        let dir = crate::config_dir();
        let providers = myuri_core::config::load_providers(&dir).expect("providers.toml");
        let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().parent().unwrap().join("target");
        std::fs::create_dir_all(&target).unwrap();
        for id in ["elevenlabs", "soniox"] {
            let provider = providers.iter().find(|p| p.id == id && p.speech_api.is_some())
                .unwrap_or_else(|| panic!("{id} provider"));
            if id == "elevenlabs" {
                match voices(provider) {
                    Ok(list) => {
                        for v in &list { println!("voice {} = {}", v.name, v.id); }
                    }
                    Err(e) => println!("voices failed: {e}"),
                }
            }
            if id == "soniox" {
                // Probe the shared-voices endpoint for a real voice name.
                let stored = secrets::read(&provider.id).unwrap_or_default();
                if let Some(key) = provider.keys_to_try(&stored, 0).into_iter().next() {
                    for path in ["shared-voices?model=tts-rt-v2&limit=40"] {
                        let url = format!("{}/{}", provider.base_url.trim_end_matches('/'), path);
                        let headers = vec![("Authorization".to_string(), format!("Bearer {key}"))];
                        match net::get(&url, &headers) {
                            Ok(body) => println!("{path}: {}", &String::from_utf8_lossy(&body)[..body.len().min(1200)]),
                            Err(e) => println!("{path} failed: {e}"),
                        }
                    }
                }
            }
            let model = provider.tts_models.first().cloned().unwrap_or_default();
            let wav = tts(provider, &model, "你好，我是缪里。").unwrap_or_else(|e| panic!("{id} tts: {e}"));
            let file = target.join(format!("tts-{id}.wav"));
            std::fs::write(&file, &wav).unwrap();
            println!("{id} tts {} bytes -> {}", wav.len(), file.display());
            play(wav.clone());
            std::thread::sleep(Duration::from_secs(4));
            let asr_model = provider.asr_models.first().cloned().unwrap_or_default();
            if !asr_model.is_empty() {
                match stt(provider, &asr_model, &wav) {
                    Ok(text) => println!("{id} stt: {text}"),
                    Err(e) => println!("{id} stt failed: {e}"),
                }
            }
        }
    }
}
