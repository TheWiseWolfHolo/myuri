//! Clipboard access for the capture worker: read what a copy produced, and
//! put back exactly what was there before.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardFormatNameW, GetClipboardSequenceNumber, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows::core::PCWSTR;

use crate::capture::{Content, Image, ImageFormat};
use crate::win::wide;

const CF_TEXT: u32 = 1;
const CF_BITMAP: u32 = 2;
const CF_OEMTEXT: u32 = 7;
const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const CF_DIBV5: u32 = 17;

/// Snapshots larger or slower than this are not attempted; the capture is
/// skipped instead so the user's clipboard is never left half restored.
const SNAPSHOT_BYTES: usize = 128 * 1024 * 1024;
const SNAPSHOT_TIME: Duration = Duration::from_millis(400);

pub fn sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

fn format_id(name: &str) -> u32 {
    unsafe { RegisterClipboardFormatW(PCWSTR(wide(name).as_ptr())) }
}

fn format_name(format: u32) -> String {
    let mut buffer = [0u16; 128];
    let len = unsafe { GetClipboardFormatNameW(format, &mut buffer) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

/// The clipboard, held open until dropped.
struct Open;

impl Open {
    /// Another program may hold the clipboard for a moment, so retry briefly.
    fn new(owner: HWND) -> Option<Self> {
        for attempt in 0..12 {
            if unsafe { OpenClipboard(owner) }.is_ok() {
                return Some(Self);
            }
            std::thread::sleep(Duration::from_millis(if attempt < 4 { 2 } else { 8 }));
        }
        None
    }

    fn formats(&self) -> Vec<u32> {
        let mut formats = Vec::new();
        let mut format = 0;
        loop {
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                return formats;
            }
            formats.push(format);
        }
    }

    /// Bytes of a memory-backed format. Asking for a format makes its owner
    /// render it if it was promised lazily.
    fn bytes(&self, format: u32) -> Option<Vec<u8>> {
        unsafe {
            let handle = GetClipboardData(format).ok()?;
            let memory = HGLOBAL(handle.0);
            let size = GlobalSize(memory);
            let pointer = GlobalLock(memory);
            if pointer.is_null() {
                return None;
            }
            let bytes = std::slice::from_raw_parts(pointer.cast::<u8>(), size).to_vec();
            let _ = GlobalUnlock(memory);
            Some(bytes)
        }
    }

    fn set(&self, format: u32, bytes: &[u8]) -> bool {
        unsafe {
            let Ok(memory) = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) else { return false };
            let pointer = GlobalLock(memory);
            if pointer.is_null() {
                let _ = GlobalFree(memory);
                return false;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len());
            let _ = GlobalUnlock(memory);
            // On success the system owns the memory.
            let placed = SetClipboardData(format, HANDLE(memory.0)).is_ok();
            if !placed {
                let _ = GlobalFree(memory);
            }
            placed
        }
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Put a picture on the clipboard, as the registered PNG format plus a DIB
/// when the bytes decode (so older programs can paste it too).
pub fn write_image(owner: HWND, bytes: &[u8]) -> bool {
    let Some(clipboard) = Open::new(owner) else { return false };
    unsafe { let _ = EmptyClipboard(); }
    let mut placed = false;
    if bytes.starts_with(b"\x89PNG") {
        placed |= clipboard.set(format_id("PNG"), bytes);
    }
    // A DIB is a BMP file without its 14-byte header.
    if let Ok(bitmap) = ::image::load_from_memory(bytes) {
        let mut bmp = std::io::Cursor::new(Vec::new());
        if bitmap.write_to(&mut bmp, ::image::ImageFormat::Bmp).is_ok() {
            let bmp = bmp.into_inner();
            if bmp.len() > 14 {
                placed |= clipboard.set(CF_DIB, &bmp[14..]);
            }
        }
    }
    placed
}

/// Everything on the clipboard that can be copied byte for byte.
pub struct Snapshot {
    formats: Vec<(u32, Vec<u8>)>,
}

/// Formats that cannot or should not be carried over as bytes.
fn skipped(format: u32, name: &str) -> bool {
    match format {
        // GDI handles, not memory. Windows rebuilds bitmaps from the DIB.
        CF_BITMAP | 3 | 9 | 14 => true,
        // Owner-drawn and display variants.
        0x80..=0x8E => true,
        // Private and GDI object ranges hold handles only their owner understands.
        0x200..=0x3FF => true,
        // OLE links back into the source program. They are slow to render and
        // meaningless once another process owns the clipboard.
        _ => matches!(
            name,
            "DataObject"
                | "Ole Private Data"
                | "Link Source"
                | "Link Source Descriptor"
                | "ObjectLink"
                | "OwnerLink"
                | "Native"
                | "Embed Source"
                | "Embedded Object"
                | "Object Descriptor"
        ),
    }
}

/// Copy the clipboard aside. `None` means it could not be captured in full,
/// in which case the caller must leave the clipboard alone.
pub fn snapshot(owner: HWND) -> Option<Snapshot> {
    let started = Instant::now();
    let clipboard = Open::new(owner)?;
    let mut formats = Vec::new();
    let mut total = 0;
    // Windows derives the other text and bitmap flavors from the first one,
    // so one member of each family is enough.
    let (mut text, mut bitmap) = (false, false);
    for format in clipboard.formats() {
        let family = match format {
            CF_TEXT | CF_OEMTEXT | CF_UNICODETEXT => Some(&mut text),
            CF_DIB | CF_DIBV5 => Some(&mut bitmap),
            _ => None,
        };
        if let Some(seen) = family {
            if std::mem::replace(seen, true) {
                continue;
            }
        } else if skipped(format, &if format >= 0xC000 { format_name(format) } else { String::new() }) {
            continue;
        }
        let Some(bytes) = clipboard.bytes(format) else { continue };
        total += bytes.len();
        if total > SNAPSHOT_BYTES || started.elapsed() > SNAPSHOT_TIME {
            return None;
        }
        formats.push((format, bytes));
    }
    Some(Snapshot { formats })
}

/// Replace the clipboard with a snapshot.
pub fn restore(owner: HWND, snapshot: &Snapshot) -> bool {
    // SetClipboardData fails without an owning window.
    let Some(clipboard) = Open::new(owner) else { return false };
    if unsafe { EmptyClipboard() }.is_err() {
        return false;
    }
    let mut complete = true;
    for (format, bytes) in &snapshot.formats {
        complete &= clipboard.set(*format, bytes);
    }
    // The original copy is already in the clipboard history; keep this
    // restore from adding a duplicate or syncing to other devices.
    clipboard.set(format_id("ExcludeClipboardContentFromMonitorProcessing"), &[0]);
    complete
}

/// What the clipboard holds, in the order a person would mean it: files, then
/// text, then an image.
///
/// With `fresh_copy`, the content came from a copy command this program just
/// sent, and an editor's "copy the whole line when nothing is selected" is
/// reported as nothing.
pub fn read(owner: HWND, fresh_copy: bool) -> Option<Content> {
    let clipboard = Open::new(owner)?;
    let formats = clipboard.formats();
    let has = |format: u32| formats.contains(&format);

    if has(CF_HDROP) {
        let files = clipboard.bytes(CF_HDROP).map(|bytes| parse_file_list(&bytes)).unwrap_or_default();
        if !files.is_empty() {
            return Some(Content::Files(files));
        }
    }
    if has(CF_UNICODETEXT) {
        let text = clipboard.bytes(CF_UNICODETEXT).map(|bytes| decode_utf16(&bytes)).unwrap_or_default();
        if !text.trim().is_empty() {
            let custom = format_id("Chromium Web Custom MIME Data Format");
            if fresh_copy && has(custom) && clipboard.bytes(custom).is_some_and(|b| is_empty_selection_copy(&b)) {
                return None;
            }
            return Some(Content::Text(text));
        }
    }
    let png = format_id("PNG");
    if has(png) {
        if let Some(bytes) = clipboard.bytes(png).filter(|b| !b.is_empty()) {
            return Some(Content::Image(Image { format: ImageFormat::Png, bytes }));
        }
    }
    if has(CF_DIB) || has(CF_DIBV5) || has(CF_BITMAP) {
        if let Some(bytes) = clipboard.bytes(CF_DIB).filter(|b| !b.is_empty()) {
            return Some(Content::Image(Image { format: ImageFormat::Dib, bytes }.into_png()));
        }
    }
    None
}

/// UTF-16 up to the first NUL.
fn decode_utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|&unit| unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// `DROPFILES`: a 20-byte header whose first field is the offset of a
/// double-NUL-terminated list of paths.
fn parse_file_list(bytes: &[u8]) -> Vec<PathBuf> {
    if bytes.len() < 20 {
        return Vec::new();
    }
    let offset = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let wide = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) != 0;
    let Some(list) = bytes.get(offset..) else { return Vec::new() };
    if wide {
        let units: Vec<u16> =
            list.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        units
            .split(|&unit| unit == 0)
            .take_while(|path| !path.is_empty())
            .map(|path| PathBuf::from(String::from_utf16_lossy(path)))
            .collect()
    } else {
        list.split(|&byte| byte == 0)
            .take_while(|path| !path.is_empty())
            .map(|path| PathBuf::from(String::from_utf8_lossy(path).into_owned()))
            .collect()
    }
}

/// VS Code and editors built on it copy the whole line when nothing is
/// selected, and say so in the metadata they attach to the copy.
fn is_empty_selection_copy(custom_data: &[u8]) -> bool {
    let marker: Vec<u8> =
        "\"isFromEmptySelection\":true".encode_utf16().flat_map(u16::to_le_bytes).collect();
    custom_data.windows(marker.len()).any(|window| window == marker)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn text_stops_at_the_terminator() {
        let mut bytes = utf16("选中 text");
        bytes.extend([0, 0, b'x', 0]);
        assert_eq!(decode_utf16(&bytes), "选中 text");
        assert_eq!(decode_utf16(&[]), "");
    }

    #[test]
    fn file_lists_are_parsed() {
        let mut bytes = vec![0u8; 20];
        bytes[0] = 20;
        bytes[16] = 1;
        bytes.extend(utf16("C:\\a b\\一.txt\0D:\\two.png\0\0"));
        assert_eq!(
            parse_file_list(&bytes),
            [PathBuf::from("C:\\a b\\一.txt"), PathBuf::from("D:\\two.png")]
        );

        let mut ansi = vec![0u8; 20];
        ansi[0] = 20;
        ansi.extend(b"C:\\x.txt\0\0");
        assert_eq!(parse_file_list(&ansi), [PathBuf::from("C:\\x.txt")]);

        assert!(parse_file_list(&[0; 8]).is_empty());
        let mut bad_offset = vec![0u8; 20];
        bad_offset[0] = 200;
        assert!(parse_file_list(&bad_offset).is_empty());
    }

    #[test]
    fn empty_selection_copies_are_recognized() {
        let line_copy = utf16("vscode-editor-data{\"version\":1,\"isFromEmptySelection\":true,\"mode\":\"rust\"}");
        let real_copy = utf16("vscode-editor-data{\"version\":1,\"isFromEmptySelection\":false,\"mode\":\"rust\"}");
        assert!(is_empty_selection_copy(&line_copy));
        assert!(!is_empty_selection_copy(&real_copy));
        assert!(!is_empty_selection_copy(&[]));
    }

    #[test]
    fn handles_and_ole_links_are_not_snapshotted() {
        assert!(skipped(CF_BITMAP, ""));
        assert!(skipped(14, ""));
        assert!(skipped(0x250, ""));
        assert!(skipped(0xC123, "Link Source"));
        assert!(!skipped(0xC123, "HTML Format"));
        assert!(!skipped(CF_HDROP, ""));
    }
}
