#![windows_subsystem = "windows"]

mod app;
mod applog;
mod autostart;
mod brand;
mod capture;
mod chat;
mod cli;
mod clipboard;
mod events;
mod historyview;
mod hook;
mod icon;
mod images;
mod keys;
mod mcp;
mod models;
mod settings;
mod net;
mod panel;
mod picker;
mod secrets;
mod shell;
mod syncview;
mod speech;
mod theme;
mod toolsview;
mod tray;
mod traymenu;
mod uia;
mod updates;
mod vertex;
mod watch;
mod win;
mod window;

use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::w;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `%APPDATA%\Myuri`, where the configuration files live.
pub fn config_dir() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("Myuri")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = cli::dispatch(&args) {
        std::process::exit(code);
    }
    applog::init();
    // Held until the process exits.
    let _instance = match single_instance() {
        Ok(Some(instance)) => instance,
        Ok(None) => {
            let sent = tray::send_to_running_instance(tray::Request::Settings);
            applog::info!("already running; settings request sent: {sent}");
            return;
        }
        Err(error) => {
            applog::info!("cannot check for a running instance: {error}");
            return;
        }
    };
    app::run();
}

fn single_instance() -> windows::core::Result<Option<HANDLE>> {
    unsafe {
        let mutex = CreateMutexW(None, true, w!(r"Local\Myuri.SingleInstance"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(mutex);
            Ok(None)
        } else {
            Ok(Some(mutex))
        }
    }
}
