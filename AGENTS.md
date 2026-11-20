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
cargo test --workspace      # everything
cargo check --workspace     # compile-check the workspace
cargo build                 # target/debug/myuri.exe
cargo build --release       # target/release/myuri.exe
myuri check                 # validate the configuration files
myuri --version             # show the Cargo version
myuri preview [text|image|files]   # show the panel in the running instance
myuri key [--image|--speech] <id> [--delete]
myuri image <id> <model> <prompt> [--resolution 1K|2K|4K] [--ref <image>]...
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
- Configuration in `%APPDATA%\Myuri` includes `config.toml` and
  `assistants.toml`. Unknown keys and out-of-range values are errors. Files
  reload on save. Update the corresponding `DEFAULT_*` template when changing
  its schema and keep the default-equality tests passing.
- Each channel type has its own file and Windows credential target prefix.

  | Type | File | Credential target |
  | --- | --- | --- |
  | Chat | `providers.toml` | `Myuri/provider/<id>` |
  | Image | `image_providers.toml` | `Myuri/image/<id>` |
  | Speech | `speech_providers.toml` | `Myuri/speech/<id>` |

  The files can use the same IDs. Startup fills missing image and speech files
  from the legacy `providers.toml` and copies each corresponding chat secret
  when its target is empty. Verify migration with synthetic data in a temporary
  directory. Use fixture credentials and avoid reading or editing the real
  configuration during development.
- Log: `%LOCALAPPDATA%\Myuri\logs\myuri.log`. `[log] debug = true` adds timings.
- GPUI comes through `gpui-kit 0.10.0`, which pins `gpui-pre =0.3.12`; the
  `windows` crate stays at 0.58 to match it. Dev builds use `opt-level = 1`
  because GPUI is unusable unoptimized.

## Where things stand

- Done: gesture, capture, panel, tray, CLI, config hot reload, app icon
  (`tools/make-icon.py`), provider routing (`chat.rs`), streaming over WinHTTP
  (`net.rs`), four chat APIs in `myuri-core/src/llm.rs`, keys in the Windows
  credential store (`myuri key`), the assistant window (`window.rs`:
  frameless, streaming Markdown, follow-up, stop, copy, retry, model and
  reasoning switch), the model catalog (`myuri-core/src/catalog.rs`, data from
  models.dev, built-in copy in `assets/models.json`, `myuri models refresh`,
  and vendor model selection in `settings/catalog_form.rs`).
- Also done: per-provider model fetch (`myuri models fetch`), several keys
  per provider with notes and strategies, Vertex service-account sign-in
  (`vertex.rs`), picture generation for OpenAI, xAI, Gemini, EvoLink and
  WaveSpeed (`myuri-core/src/image.rs`, `images.rs`, `myuri image ...`), the
  tray menu drawn in GPUI (`traymenu.rs`), and the settings window
  (`settings.rs`: assistants, chat, pictures, speech, general; create, rename,
  delete, drag to reorder, search). Tray left click opens settings.
- `Provider`, `ImageProvider` and `SpeechProvider` describe the three channel
  types. `PickerEvent::Chosen` carries a `Purpose` together with its model
  reference. Legacy fields on `Provider` are parsed for migration. Settings
  edits go through `myuri-core/src/edit.rs` (toml_edit preserves comments).
  Reordering must reset table positions.
- The assistant window uses `ImageSpec` for generation and reference image
  editing. It also supports direct speech synthesis, WAV and MP3 audio cards,
  and saving or reading chat replies aloud. Speech protocol and playback live
  in `speech.rs` in the respective crates. The model's thinking is shown above
  its reply. `ui-state.toml` stores window state. Brand icons use
  `myuri-core/src/brand.rs` for matching and `myuri/src/brand.rs` for downloads
  and caching. `tools/lobehub-map.py` regenerates `assets/lobehub.json`.
- Also done: saved conversations (`myuri-core/src/history.rs`, the history
  page `historyview.rs`, resume through `UiMsg::Resume`), backup to a file or
  WebDAV with keys sealed by a password (`myuri-core/src/backup.rs`,
  `syncview.rs`), and tools: tool calls in all four dialects (`llm.rs`:
  `Tool`, `ToolCall`, `Event::Call`, `echo` carries what a provider wants
  handed back), MCP over stdio and HTTP plus skills (`myuri-core/src/mcp.rs`
  for config and wording, `myuri/src/mcp.rs` for sessions and `Kit`), the tool
  loop with confirmation in `window.rs` (`advance`, `run_call`, `decide`) and
  the tools page `toolsview.rs`.
- Settings pages that are views of their own (history, sync, tools) are
  entities held by `SettingsView`; `Tab::own_page` switches the layout.
  Windows reach the main loop with `events::post`.
- To exercise UI without touching the user's data, run the debug build with
  `APPDATA` pointing at a scratch folder (quit the running instance first),
  and fake servers on localhost for providers, MCP and WebDAV.
- The repository is public at github.com/TheWiseWolfHolo/myuri. Nothing
  personal goes into commits: no provider names, keys, paths or screenshots of
  the user's own setup. CI runs on every push. The manual Release workflow
  runs tests and uploads the Windows ZIP as an Actions artifact. A tag push
  matching the workspace version publishes a release. The branch
  `local-history` holds the pre-publication history and is never pushed.
- Only chat completions has been exercised against a real endpoint. Responses,
  Anthropic and Gemini have unit tests only.
- The user runs an installed release binary outside the repository (not
  `dist/myuri.exe`). Rebuild into `target/` freely; replace the installed
  binary only after `myuri quit`, then restart it.
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
