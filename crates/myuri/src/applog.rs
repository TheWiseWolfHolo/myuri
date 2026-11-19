//! File log at `%LOCALAPPDATA%\Myuri\logs\myuri.log`.
//!
//! The log records what happened and how long it took. It never records
//! captured content. One previous file is kept when the log passes 1 MB.

use std::fmt::Arguments;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::System::SystemInformation::GetLocalTime;

const MAX_BYTES: u64 = 1024 * 1024;

struct Log {
    path: PathBuf,
    file: File,
    bytes: u64,
}

static LOG: Mutex<Option<Log>> = Mutex::new(None);
static DEBUG: AtomicBool = AtomicBool::new(false);

pub fn path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Myuri")
        .join("logs")
        .join("myuri.log")
}

pub fn init() {
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Some(log) = open(path) {
        *LOG.lock().unwrap_or_else(|e| e.into_inner()) = Some(log);
    }
    std::panic::set_hook(Box::new(|info| write(format_args!("panic: {info}"))));
}

fn open(path: PathBuf) -> Option<Log> {
    let file = OpenOptions::new().create(true).append(true).open(&path).ok()?;
    let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    Some(Log { path, file, bytes })
}

pub fn set_debug(enabled: bool) {
    DEBUG.store(enabled, Ordering::Relaxed);
}

pub fn debug_enabled() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

pub fn write(message: Arguments<'_>) {
    let t = unsafe { GetLocalTime() };
    let line = format!(
        "[{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}] {message}\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    );
    let mut guard = LOG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(log) = guard.as_mut() else { return };
    if log.bytes > MAX_BYTES {
        let previous = log.path.with_extension("log.1");
        let _ = fs::remove_file(&previous);
        if fs::rename(&log.path, &previous).is_ok() {
            if let Some(fresh) = open(log.path.clone()) {
                *log = fresh;
            }
        }
    }
    if log.file.write_all(line.as_bytes()).is_ok() {
        log.bytes += line.len() as u64;
    }
}

macro_rules! info {
    ($($arg:tt)*) => { $crate::applog::write(format_args!($($arg)*)) };
}

/// Timings and decisions, written only when `[log] debug = true`.
macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::applog::debug_enabled() {
            $crate::applog::write(format_args!($($arg)*))
        }
    };
}

pub(crate) use {debug, info};
