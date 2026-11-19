//! Hotkey strings such as `Alt+Space`, parsed into modifier bits and a
//! virtual-key code. The bit values match Win32 `MOD_*` so the app can pass
//! them straight to `RegisterHotKey`.

pub const MOD_ALT: u32 = 0x1;
pub const MOD_CTRL: u32 = 0x2;
pub const MOD_SHIFT: u32 = 0x4;
pub const MOD_WIN: u32 = 0x8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: u32,
    pub vk: u32,
}

/// Parse a binding. An empty string means "no hotkey" and yields `Ok(None)`.
pub fn parse(binding: &str) -> Result<Option<Hotkey>, String> {
    if binding.trim().is_empty() {
        return Ok(None);
    }
    let mut modifiers = 0;
    let mut vk = None;
    for part in binding.split('+').map(str::trim) {
        let lower = part.to_ascii_lowercase();
        match lower.as_str() {
            "" => return Err(format!("快捷键 \"{binding}\" 里有空的按键")),
            "alt" => modifiers |= MOD_ALT,
            "ctrl" | "control" => modifiers |= MOD_CTRL,
            "shift" => modifiers |= MOD_SHIFT,
            "win" | "super" | "meta" => modifiers |= MOD_WIN,
            key => {
                let code = key_code(key).ok_or_else(|| format!("不认识的按键 \"{part}\""))?;
                if vk.replace(code).is_some() {
                    return Err(format!("快捷键 \"{binding}\" 只能有一个主键"));
                }
            }
        }
    }
    let vk = vk.ok_or_else(|| format!("快捷键 \"{binding}\" 缺少主键"))?;
    let is_function_key = (0x70..=0x87).contains(&vk);
    if modifiers == 0 && !is_function_key {
        return Err(format!("快捷键 \"{binding}\" 至少要带一个 Ctrl、Alt、Shift 或 Win"));
    }
    Ok(Some(Hotkey { modifiers, vk }))
}

fn key_code(key: &str) -> Option<u32> {
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return match c {
            'a'..='z' => Some(c.to_ascii_uppercase() as u32),
            '0'..='9' => Some(c as u32),
            '`' => Some(0xC0),
            '-' => Some(0xBD),
            '=' => Some(0xBB),
            '[' => Some(0xDB),
            ']' => Some(0xDD),
            '\\' => Some(0xDC),
            ';' => Some(0xBA),
            '\'' => Some(0xDE),
            ',' => Some(0xBC),
            '.' => Some(0xBE),
            '/' => Some(0xBF),
            _ => None,
        };
    }
    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        return (1..=24).contains(&n).then(|| 0x70 + n - 1);
    }
    Some(match key {
        "space" => 0x20,
        "tab" => 0x09,
        "enter" | "return" => 0x0D,
        "esc" | "escape" => 0x1B,
        "backspace" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "plus" => 0xBB,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_bindings() {
        assert_eq!(parse("Alt+Space"), Ok(Some(Hotkey { modifiers: MOD_ALT, vk: 0x20 })));
        assert_eq!(
            parse(" ctrl + ALT + space "),
            Ok(Some(Hotkey { modifiers: MOD_CTRL | MOD_ALT, vk: 0x20 }))
        );
        assert_eq!(
            parse("Win+Shift+A"),
            Ok(Some(Hotkey { modifiers: MOD_WIN | MOD_SHIFT, vk: 'A' as u32 }))
        );
        assert_eq!(parse("Ctrl+`"), Ok(Some(Hotkey { modifiers: MOD_CTRL, vk: 0xC0 })));
        assert_eq!(parse("F8"), Ok(Some(Hotkey { modifiers: 0, vk: 0x77 })));
        assert_eq!(parse("Ctrl+F24"), Ok(Some(Hotkey { modifiers: MOD_CTRL, vk: 0x87 })));
    }

    #[test]
    fn empty_disables() {
        assert_eq!(parse(""), Ok(None));
        assert_eq!(parse("   "), Ok(None));
    }

    #[test]
    fn rejects_bad_bindings() {
        assert!(parse("Alt+Nope").is_err());
        assert!(parse("Alt+").is_err());
        assert!(parse("Alt").is_err());
        assert!(parse("Ctrl+A+B").is_err());
        assert!(parse("F25").is_err());
        // A bare letter would swallow ordinary typing.
        assert!(parse("A").is_err());
    }
}
