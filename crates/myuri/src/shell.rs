//! Files selected in File Explorer or on the desktop, asked from the shell
//! directly.

use std::path::PathBuf;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, IServiceProvider};
use windows::Win32::UI::Shell::{
    IFolderView, IShellBrowser, IShellItemArray, IShellWindows, IWebBrowserApp,
    SID_STopLevelBrowser, SIGDN_FILESYSPATH, SVGIO_SELECTION, SWC_DESKTOP, SWFO_NEEDDISPATCH,
    ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::FindWindowExW;
use windows::core::{Interface, PCWSTR, VARIANT, w};

use crate::win::WindowInfo;

fn is_desktop(window: &WindowInfo) -> bool {
    matches!(window.class.as_str(), "Progman" | "WorkerW")
}

/// A folder window or the desktop.
pub fn is_file_view(window: &WindowInfo) -> bool {
    window.exe == "explorer.exe"
        && (is_desktop(window) || matches!(window.class.as_str(), "CabinetWClass" | "ExploreWClass"))
}

/// Paths of the selected items; empty when nothing is selected or the shell
/// could not be asked. Requires COM to be initialized on the calling thread.
pub fn selected_files(window: &WindowInfo) -> Vec<PathBuf> {
    unsafe { browser_for(window).and_then(|browser| selection(&browser)) }.unwrap_or_default()
}

unsafe fn browser_for(window: &WindowInfo) -> Option<IShellBrowser> {
    unsafe {
        let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok()?;
        if is_desktop(window) {
            let mut hwnd = 0;
            let desktop = windows
                .FindWindowSW(
                    &VARIANT::from(0i32),
                    &VARIANT::default(),
                    SWC_DESKTOP,
                    &mut hwnd,
                    SWFO_NEEDDISPATCH,
                )
                .ok()?;
            return top_level_browser(&desktop);
        }

        // With tabs, every tab is a shell window that reports the same
        // top-level handle. The visible tab is the first tab child.
        let active_tab = FindWindowExW(
            window.handle(),
            HWND::default(),
            w!("ShellTabWindowClass"),
            PCWSTR::null(),
        )
        .ok()
        .filter(|tab| !tab.0.is_null());
        for index in 0..windows.Count().ok()? {
            let Ok(item) = windows.Item(&VARIANT::from(index)) else { continue };
            let Ok(app) = item.cast::<IWebBrowserApp>() else { continue };
            if app.HWND().map(|h| h.0 as isize).ok() != Some(window.hwnd) {
                continue;
            }
            let Some(browser) = top_level_browser(&app) else { continue };
            if active_tab.is_none() || browser.GetWindow().ok() == active_tab {
                return Some(browser);
            }
        }
        None
    }
}

unsafe fn top_level_browser(object: &impl Interface) -> Option<IShellBrowser> {
    unsafe {
        object.cast::<IServiceProvider>().ok()?.QueryService(&SID_STopLevelBrowser).ok()
    }
}

unsafe fn selection(browser: &IShellBrowser) -> Option<Vec<PathBuf>> {
    unsafe {
        let view = browser.QueryActiveShellView().ok()?.cast::<IFolderView>().ok()?;
        let items: IShellItemArray = view.Items(SVGIO_SELECTION).ok()?;
        let mut paths = Vec::new();
        for index in 0..items.GetCount().ok()? {
            // Virtual items such as "This PC" have no file system path.
            let Ok(name) = items.GetItemAt(index).and_then(|item| item.GetDisplayName(SIGDN_FILESYSPATH))
            else {
                continue;
            };
            if let Ok(path) = name.to_string() {
                paths.push(PathBuf::from(path));
            }
            CoTaskMemFree(Some(name.0.cast()));
        }
        Some(paths)
    }
}
