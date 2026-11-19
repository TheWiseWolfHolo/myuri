//! Reading the selection through UI Automation: no keystrokes, no clipboard.
//!
//! Only used for programs where asking is free. Browsers and Electron apps
//! switch their whole accessibility machinery on when a client like this
//! shows up, so the capture worker never calls in here for them.

use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement,
    IUIAutomationTextPattern, UIA_TextPatternId,
};

/// A hung program must not stall the capture.
const TIMEOUT_MS: u32 = 250;
/// Selections longer than this are cut; nobody sends a novel to an assistant.
const MAX_CHARS: i32 = 200_000;
/// Some programs put the text pattern on a container of the focused element.
const ANCESTORS: usize = 3;

pub enum Selection {
    Text(String),
    /// The focused control exposes its text and nothing is selected.
    Empty,
    /// The focused control does not expose text this way.
    Unsupported,
}

pub struct Uia {
    automation: IUIAutomation,
}

impl Uia {
    /// Requires COM to be initialized on the calling thread.
    pub fn new() -> Option<Self> {
        unsafe {
            if let Ok(automation) =
                CoCreateInstance::<_, IUIAutomation2>(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)
            {
                let _ = automation.SetConnectionTimeout(TIMEOUT_MS);
                let _ = automation.SetTransactionTimeout(TIMEOUT_MS);
                return Some(Self { automation: automation.into() });
            }
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                .ok()
                .map(|automation| Self { automation })
        }
    }

    pub fn selection(&self) -> Selection {
        unsafe {
            let Ok(mut element) = self.automation.GetFocusedElement() else {
                return Selection::Unsupported;
            };
            let walker = self.automation.ControlViewWalker().ok();
            for _ in 0..=ANCESTORS {
                if let Some(pattern) = text_pattern(&element) {
                    return read(&pattern);
                }
                let parent = walker.as_ref().and_then(|w| w.GetParentElement(&element).ok());
                match parent {
                    Some(parent) => element = parent,
                    None => break,
                }
            }
            Selection::Unsupported
        }
    }
}

unsafe fn text_pattern(element: &IUIAutomationElement) -> Option<IUIAutomationTextPattern> {
    unsafe { element.GetCurrentPatternAs(UIA_TextPatternId).ok() }
}

unsafe fn read(pattern: &IUIAutomationTextPattern) -> Selection {
    unsafe {
        let Ok(ranges) = pattern.GetSelection() else { return Selection::Unsupported };
        let mut text = String::new();
        for index in 0..ranges.Length().unwrap_or(0) {
            let Ok(part) = ranges.GetElement(index).and_then(|range| range.GetText(MAX_CHARS))
            else {
                continue;
            };
            if !text.is_empty() && !part.is_empty() {
                text.push('\n');
            }
            text.push_str(&part.to_string());
        }
        if text.trim().is_empty() { Selection::Empty } else { Selection::Text(text) }
    }
}
