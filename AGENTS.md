# Myuri

Windows desktop AI assistant: hold the right mouse button, or press a hotkey,
to capture the selection and pick an assistant from a panel at the cursor.

## Layout

- `crates/myuri-core`: pure logic, no Win32 and no UI. `gesture.rs` (hold
  gesture state machine), `config.rs` (TOML schema, validation, shipped
  defaults), `hotkey.rs`, `matching.rs` (name and pinyin filter).
- `crates/myuri`: the application.
  - `hook.rs`: low-level mouse hook and global hotkey thread.
  - `capture.rs`: capture worker, using `uia.rs`, `shell.rs`, `keys.rs`,
    `clipboard.rs`.
  - `panel.rs`, `theme.rs`: the GPUI panel.
  - `tray.rs`: tray icon, menu, notifications, and the control window the CLI
    talks to. `watch.rs`: configuration hot reload. `app.rs`: wiring.
  - `cli.rs`, `applog.rs`, `autostart.rs`, `win.rs`.

## Commands

```sh
cargo test -p myuri-core    # seconds; no GPUI build
cargo test                  # everything
cargo build                 # target/debug/myuri.exe
myuri check                 # validate the configuration files
myuri preview [text|image|files]   # show the panel in the running instance
myuri quit
```

`myuri preview` shows the panel with stand-in content and reads nothing, so it
is safe for checking layout. The real gesture and capture paths need a person
at the mouse; say so instead of claiming they were verified.

## Rules

- The hook callback never blocks: run the state machine, inject input, post
  messages. Windows silently removes a slow low-level hook.
- Never send Ctrl+C to a terminal, and never call UI Automation on Chromium or
  Firefox windows. Both rules live in `capture.rs` with their reasons.
- The clipboard is restored exactly or not touched at all.
- Logs never contain captured content, only kinds and sizes.
- Configuration is `%APPDATA%\Myuri\config.toml` and `assistants.toml`. Unknown
  keys and out-of-range values are errors. Files reload on save. When changing
  the schema, update `DEFAULT_SETTINGS` / `DEFAULT_ASSISTANTS` with it; a test
  keeps them equal to the code.
- Log: `%LOCALAPPDATA%\Myuri\logs\myuri.log`. `[log] debug = true` adds timings.
- GPUI comes through `gpui-kit 0.10.0`, which pins `gpui-pre =0.3.12`; the
  `windows` crate stays at 0.58 to match it. Dev builds use `opt-level = 1`
  because GPUI is unusable unoptimized.

## Where things stand

- Done: gesture, capture, panel, tray, CLI, config hot reload, app icon
  (`tools/make-icon.py`), provider routing (`chat.rs`), streaming over WinHTTP
  (`net.rs`), four chat APIs in `myuri-core/src/llm.rs`, keys in the Windows
  credential store (`myuri key <id>`), the assistant window (`window.rs`:
  frameless, streaming Markdown, follow-up, stop, copy, retry, model and
  reasoning switch), the model catalog (`myuri-core/src/catalog.rs`, data from
  models.dev, built-in copy in `assets/models.json`, `myuri models refresh`).
- The user's real data is in `%APPDATA%\Myuri`; never print keys.
- Also done: per-provider model fetch (`myuri models fetch`), several keys
  per provider with notes and strategies, Vertex service-account sign-in
  (`vertex.rs`), picture generation for OpenAI, xAI, Gemini, EvoLink and
  WaveSpeed (`myuri-core/src/image.rs`, `images.rs`, `myuri image ...`), the
  tray menu drawn in GPUI (`traymenu.rs`), and the settings window
  (`settings.rs`: assistants, chat, pictures, speech, general; create, rename,
  delete, drag to reorder, search). Tray left click opens settings.
- A provider serves chat, pictures (`image_models`, `image_api`) and/or speech
  (`speech_api`, `asr_models`, `tts_models`); `Provider::serves` decides which
  settings page lists it. Settings edits go through `myuri-core/src/edit.rs`
  (toml_edit, comments kept; reordering must reset table positions).
- Also done: pictures and speech in the assistant window (`speech.rs` in both
  crates), the model's thinking shown above its reply, a shared model picker
  (`picker.rs`), `ui-state.toml` for window state, and brand icons
  (`myuri-core/src/brand.rs` rules, `myuri/src/brand.rs` download and cache;
  `tools/lobehub-map.py` regenerates `assets/lobehub.json`).
- Not done yet: history, MCP tools and skills, WebDAV sync, settings
  import/export.
- The repository is public at github.com/TheWiseWolfHolo/myuri. Nothing
  personal goes into commits: no provider names, keys, paths or screenshots of
  the user's own setup. CI runs on every push; pushing a tag `vX.Y.Z` that
  matches the workspace version builds and publishes a release. The branch
  `local-history` holds the pre-publication history and is never pushed.
- Only chat completions has been exercised against a real endpoint. Responses,
  Anthropic and Gemini have unit tests only.
- `dist/myuri.exe` is the copy the user runs day to day. Rebuild into
  `target/` freely; copy to `dist/` only after `myuri quit`, then restart it.
- `tools/ui-probe.ps1` posts keys and clicks to Myuri's own windows and saves
  screenshots to `target/`; `-Drag x1,y1,x2,y2` drags. Coordinates are
  physical pixels. It pops windows up on the user's screen: say so before
  using it, and close what it opened.
- Exec commands longer than about 8000 characters are cut off on this machine:
  write long patch scripts to a file and run that.

## GPUI notes (gpui-kit 0.10.0)

- `gpui_kit::*` is GPUI itself; components are `gpui_kit::component::*`.
  Call `gpui_kit::init(cx)` once before using any component, and open windows
  that contain components with `gpui_kit::open_window`, which wraps the view
  in the `Root` the components need. The panel uses plain `cx.open_window`
  because it has no components.
- Streaming Markdown: `cx.new(|cx| TextViewState::markdown("", cx))`, then
  `state.update(cx, |s, cx| s.push_str(chunk, cx))`; render with
  `TextView::new(&state)` from `component::text`. Math is parsed but not
  rendered; code highlighting is opt-in.
- Multi-line input: `TextareaState::new(window, cx).auto_grow(min, max)` with
  `Textarea::new(&state)`; single line is `InputState` / `Input`. Both emit
  `InputEvent::{Change, PressEnter { secondary, shift }, Focus, Blur}`; read
  the text with `.value()`.
- Theme: `component::theme::Theme::global_mut(cx)` has `colors`,
  `font_family`, `mono_font_family`, `font_size`, `radius`;
  `Theme::change(mode, None, cx)` switches light and dark.
- `WindowKind::PopUp` is topmost and stays off the taskbar. Raw `HWND` comes
  from `raw_window_handle::HasWindowHandle` on `Window`.
- Font families on this machine: `Anthropic Sans Text`, `Anthropic Serif Text`,
  `Anthropic Mono Web`, `Noto Sans SC`, `Noto Serif SC`.
- Shell heredocs longer than about 10 KB get cut off in this environment:
  write large files with the file tool, or a patch script saved under
  `target/` and deleted after use.
