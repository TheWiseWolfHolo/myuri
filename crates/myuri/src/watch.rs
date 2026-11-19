//! Tells the UI thread when a file in the configuration folder changes.

use std::path::PathBuf;
use std::time::Duration;

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FindFirstChangeNotificationW, FindNextChangeNotification,
};
use windows::Win32::System::Threading::{INFINITE, WaitForSingleObject};
use windows::core::PCWSTR;

use crate::applog::info;
use crate::events::{UiMsg, UiSender};
use crate::win::wide;

pub fn spawn(dir: PathBuf, ui: UiSender) {
    let spawned = std::thread::Builder::new().name("myuri-watch".into()).spawn(move || unsafe {
        let path = wide(&dir.to_string_lossy());
        let filter =
            FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_SIZE;
        let handle: HANDLE = match FindFirstChangeNotificationW(PCWSTR(path.as_ptr()), false, filter)
        {
            Ok(handle) => handle,
            Err(error) => {
                info!("config: cannot watch {}: {error}", dir.display());
                return;
            }
        };
        while WaitForSingleObject(handle, INFINITE) == WAIT_OBJECT_0 {
            // Editors write in several steps; report once they have settled.
            loop {
                std::thread::sleep(Duration::from_millis(150));
                if FindNextChangeNotification(handle).is_err() {
                    return;
                }
                if WaitForSingleObject(handle, 0) != WAIT_OBJECT_0 {
                    break;
                }
            }
            ui.send(UiMsg::ConfigChanged);
        }
    });
    if let Err(error) = spawned {
        info!("config: cannot start the watcher: {error}");
    }
}
