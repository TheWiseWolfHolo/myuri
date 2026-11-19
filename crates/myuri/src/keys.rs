//! Synthetic copy keystrokes for programs whose selection cannot be read
//! directly.

use std::time::{Duration, Instant};

use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MapVirtualKeyW, SendInput, VIRTUAL_KEY, VK_C, VK_CONTROL,
    VK_INSERT, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};

/// Same tag as the mouse input injected by the hook thread.
const INJECT_TAG: usize = 0x4D59_5552;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chord {
    /// The oldest copy shortcut on Windows. Unlike Ctrl+C it never interrupts
    /// a running command in a terminal.
    CtrlInsert,
    /// Ctrl+Insert and Ctrl+C in one stroke, for programs that only know one.
    CtrlInsertThenC,
    /// Copy in terminals that leave Ctrl+C to the shell.
    CtrlShiftC,
}

pub fn send(chord: Chord) {
    const DOWN: bool = true;
    const UP: bool = false;
    let keys: &[(VIRTUAL_KEY, bool)] = match chord {
        Chord::CtrlInsert => {
            &[(VK_CONTROL, DOWN), (VK_INSERT, DOWN), (VK_INSERT, UP), (VK_CONTROL, UP)]
        }
        Chord::CtrlInsertThenC => &[
            (VK_CONTROL, DOWN),
            (VK_INSERT, DOWN),
            (VK_INSERT, UP),
            (VK_C, DOWN),
            (VK_C, UP),
            (VK_CONTROL, UP),
        ],
        Chord::CtrlShiftC => &[
            (VK_CONTROL, DOWN),
            (VK_SHIFT, DOWN),
            (VK_C, DOWN),
            (VK_C, UP),
            (VK_SHIFT, UP),
            (VK_CONTROL, UP),
        ],
    };
    let inputs: Vec<INPUT> = keys.iter().map(|&(key, down)| key_input(key, down)).collect();
    unsafe {
        SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}

fn key_input(key: VIRTUAL_KEY, down: bool) -> INPUT {
    let mut flags = if down { Default::default() } else { KEYEVENTF_KEYUP };
    // Without the extended flag, Insert is the numeric keypad's 0.
    if key == VK_INSERT {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                wScan: unsafe { MapVirtualKeyW(u32::from(key.0), MAPVK_VK_TO_VSC) } as u16,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

/// Wait until no modifier key is physically down. A held Alt or Shift (from
/// the hotkey that opened the panel) would turn the copy shortcut into
/// something else.
pub fn modifiers_released(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let held = [VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN]
            .iter()
            .any(|key| unsafe { GetAsyncKeyState(i32::from(key.0)) } < 0);
        if !held {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
