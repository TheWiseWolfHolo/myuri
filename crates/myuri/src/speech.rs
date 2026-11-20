//! Speech: text to speech and speech to text through the providers named in
//! `[speech]`, playback through PlaySoundW and recording through waveIn.
//! Everything that talks to a service blocks; call it off the UI thread.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Media::Audio::{
    CALLBACK_NULL, HWAVEIN, PlaySoundW, SND_ASYNC, SND_FLAGS, SND_MEMORY, SND_NODEFAULT, SND_PURGE,
    WAVE_FORMAT_PCM, WAVE_MAPPER, WAVEHDR, WAVEFORMATEX, WHDR_DONE, waveInAddBuffer,
    waveInClose, waveInOpen, waveInPrepareHeader, waveInReset, waveInStart, waveInStop,
    waveInUnprepareHeader,
};
use windows::Win32::Media::Multimedia::{
    MCI_CLOSE, MCI_FORMAT_MILLISECONDS, MCI_OPEN, MCI_OPEN_ELEMENT, MCI_OPEN_PARMSW,
    MCI_OPEN_TYPE, MCI_PLAY, MCI_PLAY_PARMS, MCI_SET, MCI_SET_PARMS, MCI_SET_TIME_FORMAT,
    MCI_STATUS, MCI_STATUS_ITEM, MCI_STATUS_LENGTH, MCI_STATUS_PARMS, MCI_WAIT, mciSendCommandW,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::core::{PCWSTR, w};

use myuri_core::config::{SpeechApi, SpeechProvider};
use myuri_core::llm::{self, HttpRequest};
use myuri_core::speech::{self, Audio, Voice};
pub use myuri_core::speech::TtsOptions;

use crate::{net::{self, Abort}, secrets};

/// How long a Soniox transcription may take before giving up.
const PATIENCE: Duration = Duration::from_secs(90);

/// Audio ready to write to disk. Raw PCM is wrapped as WAV by `tts`.
pub struct Clip {
    pub bytes: Vec<u8>,
    pub ext: &'static str,
}

pub fn tts_options(provider: &SpeechProvider, overrides: &TtsOptions) -> TtsOptions {
    fn text(typed: &str, saved: &str) -> String {
        if typed.is_empty() { saved } else { typed }.to_string()
    }
    TtsOptions {
        voice: text(&overrides.voice, &provider.voice),
        stability: overrides.stability.or(provider.stability),
        similarity_boost: overrides.similarity_boost.or(provider.similarity_boost),
        style: overrides.style.or(provider.style),
        speed: overrides.speed.or(provider.speed),
        language: text(&overrides.language, &provider.language),
        output_format: text(&overrides.output_format, &provider.output_format),
    }
}

fn with_key<T>(
    provider: &SpeechProvider,
    stored_override: Option<&str>,
    mut work: impl FnMut(&str) -> Result<T, (String, bool)>,
) -> Result<T, String> {
    static TURNS: OnceLock<Mutex<BTreeMap<String, usize>>> = OnceLock::new();
    let stored = stored_override.map(str::to_string).or_else(|| secrets::read_in(secrets::NS_SPEECH, &provider.id))
        .filter(|stored| !stored.trim().is_empty())
        .ok_or_else(|| format!("语音渠道 {} 还没有密钥", provider.id))?;
    let turn = {
        let mut turns = TURNS
            .get_or_init(|| Mutex::new(BTreeMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = turns.entry(provider.id.clone()).or_default();
        let turn = *entry;
        *entry = turn.wrapping_add(1);
        turn
    };
    let mut failure = "密钥都停用了".to_string();
    for key in provider.keys_to_try(&stored, turn) {
        match work(key) {
            Ok(value) => return Ok(value),
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

pub fn tts(provider: &SpeechProvider, model: &str, text: &str, options: &TtsOptions) -> Result<Clip, String> {
    tts_internal(provider, model, text, options, None, None)
}

pub fn tts_with_abort(
    provider: &SpeechProvider,
    model: &str,
    text: &str,
    options: &TtsOptions,
    abort: &Abort,
) -> Result<Clip, String> {
    tts_internal(provider, model, text, options, None, Some(abort))
}

/// Use the draft channel and the currently typed key for preview playback.
pub fn tts_draft(
    provider: &SpeechProvider,
    model: &str,
    text: &str,
    options: &TtsOptions,
    stored_override: Option<&str>,
) -> Result<Clip, String> {
    tts_internal(provider, model, text, options, stored_override, None)
}

fn tts_internal(
    provider: &SpeechProvider,
    model: &str,
    text: &str,
    options: &TtsOptions,
    stored_override: Option<&str>,
    abort: Option<&Abort>,
) -> Result<Clip, String> {
    let mut options = tts_options(provider, options);
    if provider.api == SpeechApi::Elevenlabs {
        options.output_format = speech::validate_elevenlabs_format(&options.output_format)?;
    }
    with_key(provider, stored_override, |key| {
        if abort.is_some_and(Abort::is_stopped) {
            return Err(("请求已停止".into(), false));
        }
        let sent = speech::tts_with_options(provider.api, &provider.base_url, key, model, text, &options);
        let request_headers = headers(provider, &sent);
        let payload = sent.body.to_string();
        let reply = match abort {
            Some(abort) => abort.post(&sent.url, &request_headers, payload.as_bytes()),
            None => net::post(&sent.url, &request_headers, payload.as_bytes()),
        };
        let body = match reply {
            Ok(pair) => response(Ok(pair))?,
            Err(reason) if abort.is_some_and(Abort::is_stopped) => return Err((reason, false)),
            Err(reason) => return Err((reason, true)),
        };
        let audio = speech::tts_audio(provider.api, &body, &options.output_format).map_err(|err| (err, false))?;
        let (bytes, ext) = match audio {
            Audio::Wav(bytes) => (normalize_wav(bytes), "wav"),
            Audio::Pcm { rate, bytes } if bytes.len().is_multiple_of(2) => (speech::wav(rate, &bytes), "wav"),
            Audio::Pcm { .. } => return Err(("PCM 音频不完整".into(), false)),
            Audio::Mp3(bytes) => (bytes, "mp3"),
        };
        Ok(Clip { bytes, ext })
    })
}

/// Transcribe a WAV file's bytes.
pub fn stt(provider: &SpeechProvider, model: &str, audio: &[u8]) -> Result<String, String> {
    with_key(provider, None, |key| {
        if provider.api == SpeechApi::Soniox {
            soniox_stt(provider, key, model, audio)
        } else {
            let (sent, body, _) = speech::stt(provider.api, &provider.base_url, key, model, audio);
            let reply = response(net::post(&sent.url, &headers(provider, &sent), &body))?;
            speech::read_text(&String::from_utf8_lossy(&reply)).map_err(|reason| (reason, false))
        }
    })
}

fn headers(provider: &SpeechProvider, request: &HttpRequest) -> Vec<(String, String)> {
    let mut headers = request.headers.clone();
    headers.extend(provider.headers.iter().map(|(n, v)| (n.clone(), v.clone())));
    headers
}

fn response(reply: Result<(u16, Vec<u8>), String>) -> Result<Vec<u8>, (String, bool)> {
    let (status, reply) = reply.map_err(|reason| (reason, true))?;
    match status {
        200..=299 => Ok(reply),
        _ => Err((
            llm::describe_failure(status, &String::from_utf8_lossy(&reply)),
            llm::another_key_may_work(status),
        )),
    }
}

/// Soniox transcribes asynchronously: upload, start, poll, fetch, clean up.
fn soniox_stt(
    provider: &SpeechProvider,
    key: &str,
    model: &str,
    audio: &[u8],
) -> Result<String, (String, bool)> {
    let base = provider.base_url.trim_end_matches('/');
    let fatal = |reason: String| (reason, false);
    let post = |request: &HttpRequest, body: &[u8]| {
        response(net::post(&request.url, &headers(provider, request), body))
    };
    let get = |request: &HttpRequest| {
        response(net::get_status(&request.url, &headers(provider, request)))
    };
    let delete = |kind, id: &str| {
        let request = speech::soniox_delete(base, key, kind, id);
        let _ = net::delete(&request.url, &headers(provider, &request));
    };

    // 1. Upload the recording.
    let (request, body, _) = speech::stt(SpeechApi::Soniox, base, key, model, audio);
    let uploaded = post(&request, &body)?;
    let file_id = speech::read_file_id(&String::from_utf8_lossy(&uploaded))
        .ok_or_else(|| fatal("上传录音没有拿到文件号".into()))?;
    let mut transcription_id = None;
    let result = (|| {
        // 2. Start the transcription.
        let request = speech::soniox_transcribe(base, key, model, &file_id);
        let submitted = post(&request, request.body.to_string().as_bytes())?;
        let id = speech::read_file_id(&String::from_utf8_lossy(&submitted))
            .ok_or_else(|| fatal("转写任务没有拿到编号".into()))?;
        let id = transcription_id.insert(id).as_str();

        // 3. Poll, then fetch the transcript.
        let started = Instant::now();
        while started.elapsed() < PATIENCE {
            let reply = get(&speech::soniox_poll(base, key, id))?;
            if speech::read_status(&String::from_utf8_lossy(&reply)).map_err(fatal)? {
                let transcript = get(&speech::soniox_transcript(base, key, id))?;
                return speech::read_text(&String::from_utf8_lossy(&transcript)).map_err(fatal);
            }
            std::thread::sleep(Duration::from_millis(800));
        }
        Err(fatal("转写等太久没出结果".into()))
    })();
    if let Some(id) = transcription_id {
        delete("transcriptions", &id);
    }
    delete("files", &file_id);
    result
}

/// The voices a provider offers; only ElevenLabs lists them.
pub fn voices(provider: &SpeechProvider) -> Result<Vec<Voice>, String> {
    voices_draft(provider, None)
}

pub fn voices_draft(provider: &SpeechProvider, stored_override: Option<&str>) -> Result<Vec<Voice>, String> {
    if provider.api != SpeechApi::Elevenlabs {
        return Ok(Vec::new());
    }
    with_key(provider, stored_override, |key| {
        let Some(request) = speech::voices(provider.api, &provider.base_url, key) else {
            return Ok(Vec::new());
        };
        let body = response(net::get_status(&request.url, &headers(provider, &request)))?;
        Ok(speech::read_voices(&String::from_utf8_lossy(&body)))
    })
}

pub fn models(
    provider: &SpeechProvider,
    stored_override: Option<&str>,
) -> Result<(Vec<String>, Vec<String>), String> {
    if speech::models_request(provider.api, &provider.base_url, "").is_none() {
        return Err("这个语音接口没有模型列表".into());
    }
    with_key(provider, stored_override, |key| {
        let request = speech::models_request(provider.api, &provider.base_url, key).unwrap();
        let reply = response(net::get_status(&request.url, &headers(provider, &request)))?;
        speech::parse_models(provider.api, &String::from_utf8_lossy(&reply)).map_err(|reason| (reason, false))
    })
}

pub fn folder() -> PathBuf {
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default().join("Music").join("Myuri")
}

pub fn save(folder: &Path, bytes: &[u8], ext: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(folder).map_err(|error| error.to_string())?;
    let mut stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    loop {
        let path = folder.join(format!("{stamp}.{ext}"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(bytes) {
                    drop(file);
                    let _ = fs::remove_file(&path);
                    return Err(error.to_string());
                }
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => stamp += 1,
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// Streaming WAV responses (such as OpenAI `/audio/speech` with `wav`) often
/// put placeholder sizes (`0xFFFFFFFF` or `0`) in the RIFF and `data` headers.
/// Patch those to actual lengths so `wav_duration` and `PlaySoundW` accept them.
pub fn normalize_wav(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.len() < 44 || !bytes.starts_with(b"RIFF") || &bytes[8..12] != b"WAVE" {
        return bytes;
    }
    let riff_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if !matches!(riff_size, 0 | u32::MAX) {
        return bytes;
    }
    let actual_riff = u32::try_from(bytes.len() - 8).unwrap_or(u32::MAX);
    bytes[4..8].copy_from_slice(&actual_riff.to_le_bytes());
    let mut offset = 12usize;
    while offset + 8 <= bytes.len() {
        let tag = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
        if tag == b"data" && matches!(size, 0 | u32::MAX) {
            let remaining = u32::try_from(bytes.len() - (offset + 8)).unwrap_or(u32::MAX);
            bytes[offset + 4..offset + 8].copy_from_slice(&remaining.to_le_bytes());
            break;
        }
        let step = 8usize
            .saturating_add(size as usize)
            .saturating_add((size as usize) % 2);
        if step == 0 || offset.saturating_add(step) > bytes.len() {
            break;
        }
        offset += step;
    }
    bytes
}

pub fn wav_duration(bytes: &[u8]) -> Result<Duration, String> {
    if bytes.len() < 44 || !bytes.starts_with(b"RIFF") || &bytes[8..12] != b"WAVE" {
        return Err("无效的 WAV 音频".into());
    }
    let raw_riff = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let streaming = matches!(raw_riff, 0 | u32::MAX);
    let mut chunks = if streaming {
        &bytes[12..]
    } else {
        bytes[8..].get(4..raw_riff as usize).ok_or("WAV 音频不完整")?
    };
    let mut rate = 0;
    let mut length = None;
    while !chunks.is_empty() {
        let header = chunks.get(..8).ok_or("WAV 音频不完整")?;
        let raw_size = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let (size, data) = if streaming && &header[..4] == b"data" && matches!(raw_size, 0 | u32::MAX) {
            (chunks.len() - 8, &chunks[8..])
        } else {
            let size = raw_size as usize;
            (size, chunks[8..].get(..size).ok_or("WAV 音频不完整")?)
        };
        match &header[..4] {
            b"fmt " if data.len() >= 16 => rate = u32::from_le_bytes(data[8..12].try_into().unwrap()),
            b"data" => length = Some(data.len()),
            _ => {}
        }
        if streaming && &header[..4] == b"data" && matches!(raw_size, 0 | u32::MAX) {
            break;
        }
        chunks = chunks[8 + size..].get(size % 2..).ok_or("WAV 音频不完整")?;
    }
    let length = length.filter(|_| rate > 0).ok_or("WAV 音频缺少格式或数据")?;
    Ok(Duration::from_secs_f64(length as f64 / f64::from(rate)))
}

/// A saved MP3 is played by Windows MCI. PlaySoundW handles WAV only.
struct Mp3Device(u32);

impl Mp3Device {
    fn open(path: &Path) -> Result<Self, String> {
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let mut open = MCI_OPEN_PARMSW {
            lpstrDeviceType: w!("mpegvideo"),
            lpstrElementName: PCWSTR(path.as_ptr()),
            ..Default::default()
        };
        let status = unsafe { mciSendCommandW(0, MCI_OPEN, (MCI_OPEN_TYPE | MCI_OPEN_ELEMENT | MCI_WAIT) as usize, (&mut open as *mut MCI_OPEN_PARMSW) as usize) };
        if status != 0 {
            return Err(format!("打不开 MP3 音频（{status}）"));
        }
        Ok(Self(open.wDeviceID))
    }

    fn play(&self) -> Result<(), String> {
        let mut parameters = MCI_PLAY_PARMS::default();
        let status = unsafe { mciSendCommandW(self.0, MCI_PLAY, 0, (&mut parameters as *mut MCI_PLAY_PARMS) as usize) };
        if status == 0 { Ok(()) } else { Err(format!("播放 MP3 失败（{status}）")) }
    }

    fn duration(&self) -> Result<Duration, String> {
        let mut format = MCI_SET_PARMS { dwTimeFormat: MCI_FORMAT_MILLISECONDS, ..Default::default() };
        let status = unsafe { mciSendCommandW(self.0, MCI_SET, (MCI_SET_TIME_FORMAT | MCI_WAIT) as usize, (&mut format as *mut MCI_SET_PARMS) as usize) };
        if status != 0 {
            return Err(format!("读取 MP3 时长失败（{status}）"));
        }
        let mut length = MCI_STATUS_PARMS { dwItem: MCI_STATUS_LENGTH as u32, ..Default::default() };
        let status = unsafe { mciSendCommandW(self.0, MCI_STATUS, (MCI_STATUS_ITEM | MCI_WAIT) as usize, (&mut length as *mut MCI_STATUS_PARMS) as usize) };
        if status == 0 { Ok(Duration::from_millis(length.dwReturn as u64)) } else { Err(format!("读取 MP3 时长失败（{status}）")) }
    }
}

impl Drop for Mp3Device {
    fn drop(&mut self) {
        unsafe { mciSendCommandW(self.0, MCI_CLOSE, MCI_WAIT as usize, 0) };
    }
}

#[derive(Default)]
struct Playing {
    wav: Vec<u8>,
    mp3: Option<Mp3Device>,
}

impl Playing {
    fn stop(&mut self) -> Result<(), String> {
        self.mp3 = None;
        // Windows versions that no longer implement SND_PURGE stop the
        // currently playing wave when passed a null sound and no flags.
        let stopped = unsafe {
            PlaySoundW(None, None, SND_PURGE).as_bool()
                || PlaySoundW(None, None, SND_FLAGS(0)).as_bool()
        };
        if stopped || self.wav.is_empty() {
            self.wav.clear();
            Ok(())
        } else {
            Err("停止播放失败".into())
        }
    }
}

enum PlaybackCommand {
    PlayWav(Vec<u8>, SyncSender<Result<(), String>>),
    PlaySaved(PathBuf, async_channel::Sender<Result<(), String>>),
    Stop,
}

impl Playing {
    fn play_wav(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        self.stop()?;
        let bytes = normalize_wav(bytes);
        wav_duration(&bytes)?;
        self.wav = bytes;
        let started = unsafe {
            PlaySoundW(
                PCWSTR(self.wav.as_ptr().cast()),
                None,
                SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
            )
            .as_bool()
        };
        if started { Ok(()) } else { Err("播放 WAV 失败".into()) }
    }

    fn play_saved(&mut self, path: &Path) -> Result<(), String> {
        if is_mp3(path) {
            self.stop()?;
            let device = Mp3Device::open(path)?;
            device.play()?;
            self.mp3 = Some(device);
            Ok(())
        } else {
            self.play_wav(fs::read(path).map_err(|error| error.to_string())?)
        }
    }
}

fn is_mp3(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mp3"))
}

/// MCI devices are bound to the thread that opened them; keep all playback
/// operations on one STA worker thread so open, play, and close always match.
fn playback_worker() -> &'static Mutex<Sender<PlaybackCommand>> {
    static WORKER: OnceLock<Mutex<Sender<PlaybackCommand>>> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<PlaybackCommand>();
        let _ = std::thread::Builder::new()
            .name("myuri-audio".into())
            .spawn(move || {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                }
                let mut slot = Playing::default();
                for command in rx {
                    match command {
                        PlaybackCommand::PlayWav(bytes, reply) => {
                            let _ = reply.send(slot.play_wav(bytes));
                        }
                        PlaybackCommand::PlaySaved(path, reply) => {
                            let _ = reply.send_blocking(slot.play_saved(&path));
                        }
                        PlaybackCommand::Stop => {
                            let _ = slot.stop();
                        }
                    }
                }
            });
        Mutex::new(tx)
    })
}

fn send_playback(command: PlaybackCommand) -> Result<(), String> {
    playback_worker()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(command)
        .map_err(|_| "音频播放线程已停止".to_string())
}

/// Play WAV bytes from memory. Another play or `silence` cuts it short.
pub fn play(bytes: Vec<u8>) -> Result<(), String> {
    let (tx, rx) = mpsc::sync_channel(1);
    send_playback(PlaybackCommand::PlayWav(bytes, tx))?;
    rx.recv().map_err(|_| "音频播放线程已停止".to_string())?
}

/// Queue playback of a saved WAV or MP3 on the dedicated audio thread without
/// blocking the caller, preserving strict FIFO order with `silence()`.
pub fn play_saved_async(path: PathBuf) -> async_channel::Receiver<Result<(), String>> {
    let (tx, rx) = async_channel::bounded(1);
    if let Err(reason) = send_playback(PlaybackCommand::PlaySaved(path, tx.clone())) {
        let _ = tx.send_blocking(Err(reason));
    }
    rx
}

#[allow(dead_code)]
pub fn play_saved(path: &Path) -> Result<(), String> {
    play_saved_async(path.to_path_buf())
        .recv_blocking()
        .map_err(|_| "音频播放线程已停止".to_string())?
}

pub fn duration_saved(path: &Path) -> Result<Duration, String> {
    if is_mp3(path) {
        Mp3Device::open(path)?.duration()
    } else {
        wav_duration(&fs::read(path).map_err(|error| error.to_string())?)
    }
}

/// Stop whatever is playing without blocking the caller.
pub fn silence() {
    let _ = send_playback(PlaybackCommand::Stop);
}

/// 16 kHz mono s16le — what speech models ask for.
const RATE: u32 = 16000;
const BUFFER: usize = (RATE as usize) * 2 / 10;

/// An open microphone recording; `stop` returns a WAV file's bytes.
pub struct Recorder {
    device: HWAVEIN,
    /// Memory the prepared headers point into; only kept alive here.
    _buffers: Vec<Box<[u8]>>,
    /// Each header is boxed so its address stays pinned for Win32 `waveIn`.
    #[allow(clippy::vec_box)]
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
        let mut prepared = 0usize;
        let cleanup = |headers: &mut [Box<WAVEHDR>], prepared: usize, message: &str| -> String {
            waveInReset(device);
            for header in &mut headers[..prepared] {
                waveInUnprepareHeader(device, &mut **header, size_of::<WAVEHDR>() as u32);
            }
            waveInClose(device);
            message.to_string()
        };
        for index in 0..headers.len() {
            if waveInPrepareHeader(device, &mut *headers[index], size_of::<WAVEHDR>() as u32) != 0 {
                return Err(cleanup(&mut headers, prepared, "麦克风准备录音缓冲失败"));
            }
            prepared += 1;
            if waveInAddBuffer(device, &mut *headers[index], size_of::<WAVEHDR>() as u32) != 0 {
                return Err(cleanup(&mut headers, prepared, "麦克风加录音缓冲失败"));
            }
        }
        if waveInStart(device) != 0 {
            return Err(cleanup(&mut headers, prepared, "麦克风开录失败"));
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

    #[test]
    fn wav_duration_checks_chunk_bounds() {
        let wav = speech::wav(16_000, &[0; 32]);
        assert_eq!(wav_duration(&wav), Ok(Duration::from_millis(1)));
        assert!(wav_duration(&wav[..75]).is_err());
        let mut malformed = wav.clone();
        malformed[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(wav_duration(&malformed).is_err());

        // OpenAI streaming WAV uses 0xFFFFFFFF placeholders in both RIFF and data sizes.
        let mut streaming = wav;
        streaming[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        streaming[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(wav_duration(&streaming), Ok(Duration::from_millis(1)));
        let patched = normalize_wav(streaming);
        assert_eq!(u32::from_le_bytes(patched[4..8].try_into().unwrap()), 68);
        assert_eq!(u32::from_le_bytes(patched[40..44].try_into().unwrap()), 32);
        assert_eq!(wav_duration(&patched), Ok(Duration::from_millis(1)));
    }

    #[test]
    fn voices_unsupported_needs_no_credentials() {
        let provider = SpeechProvider { api: SpeechApi::Soniox, ..Default::default() };
        assert_eq!(voices_draft(&provider, Some("")), Ok(Vec::new()));
    }
}
