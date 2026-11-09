# Image Importer — engineering guide for LLM agents

This file is written for an AI coding agent (or a new contributor) picking this
repository up with no prior context. It describes how the program is put
together, which rules must not be broken, and how to build, test and run it.
`README.md` is the user-facing manual: it documents behaviour, not code. When
the two disagree about behaviour, `README.md` plus the tests are the source of
truth, and the code should be corrected to match.

Everything here is Windows-specific in practice, though most of the import
engine is portable Rust.

---

## 1. What the program is

A small native Windows desktop app: pick one or more source folders and one
destination folder, press **Start import**, and every photo and video found
recursively is copied (or moved) into `<destination>/<year>/<year-month-day>/`.
The date comes from the file's metadata (EXIF, video header, or an XMP sidecar),
falling back to the file's own timestamps. An import is idempotent: re-running
it over the same card skips files that are already in the library
(byte-identical), and never overwrites anything.

Rough scale: ~8.6k lines of Rust — ~3.9k in `src/app.rs` (much of it tests),
~1.5k in `src/ui_gpui.rs`, ~830 in `src/importer.rs`, ~560 in
`src/metadata.rs`, ~850 in `src/settings.rs`, ~1k in `tests/import_test.rs`.

---

## 2. Quick facts

| Item | Value |
| --- | --- |
| Language / edition | Rust 2024 |
| Package | `image_importer` (bin + lib), version `0.1.0` |
| GUI | GPUI (`gpui-pre 0.3.11`) through [GPUI Kit](https://gpui-kit.com/) `0.9.0` |
| Other deps | `chrono`, `kamadak-exif`, `rfd` (native file dialogs), `walkdir`; `tempfile` for tests |
| Target | `x86_64-pc-windows-gnu` (pinned in `.cargo/config.toml`) |
| Binary | `target/x86_64-pc-windows-gnu/release/image_importer.exe` |
| Settings | `%APPDATA%\ImageImporter\settings.txt` |
| Run logs | `<destination>/import-log-YYYY-MM-DD_HHMMSS.txt` |
| Entry point | `src/main.rs` → `ui_gpui::ImporterWindow` → `app::ImporterApp` |
| Tests | `cargo test` (~108 unit + ~35 integration as of 2026-11) |

There is no async runtime, no database, and no network code. Background work is
one `std::thread` per import plus `std::sync::mpsc` events, and one GPUI timer
that drains them.

---

## 3. Build, test, run

The build needs a Windows linker. This machine has no Windows SDK import
libraries, so the project builds with mingw-w64 instead of MSVC:

- `.cargo/config.toml` pins `target = "x86_64-pc-windows-gnu"`.
- A folder-scoped rustup override (`stable-x86_64-pc-windows-gnu`) is active, so
  plain `cargo build` uses that toolchain.
- mingw-w64 lives in `C:\ProgramData\mingw64\mingw64\bin`. If `cargo build` fails
  at link time with "cannot find -lkernel32" and similar, put that directory on
  `PATH` first (this is what CI-less shells on this machine need):

```bash
export PATH="/c/ProgramData/mingw64/mingw64/bin:$PATH"
cargo build --release
cargo test
```

```bat
set PATH=C:\ProgramData\mingw64\mingw64\bin;%PATH%
cargo build --release
```

Output: `target/x86_64-pc-windows-gnu/release/image_importer.exe`.

Release builds set `windows_subsystem = "windows"`, so there is **no console**
and therefore no `println!` output to read. `eprintln!`/`println!` are not a
debugging channel; write to a temporary file if you must instrument, and remove
the instrumentation afterwards. Debug builds have a console (`cargo run`).

The `gpui` dependency pulls in a GPU stack; a cold build takes minutes. The
release profile is `opt-level = 2` (see `Cargo.toml`) to keep build times sane —
do not raise it without measuring.

Running the app: launch the exe, or `cargo run --release`. On a multi-monitor or
scaled (150 %) display, window sizes in the settings file are *logical* points.

Tests never read or write the real settings file: `Settings::location()` returns
`None` when `cfg!(test)`, and there is a test asserting exactly that. Keep that
property when touching settings.

---

## 4. Architecture at a glance

```
src/main.rs          application() → init(cx) → bind_keys → Settings::load
                     → open_window (theme sync) → ImporterWindow::new
                        │
src/ui_gpui.rs       ImporterWindow: the only view. Owns ImporterApp.
  (gpui)             render() paints; actions/mouse handlers call ImporterApp.
                        │  calls
src/app.rs           ImporterApp: all state, validation, log text, log file,
                     settings persistence, clipboard/Explorer/editor launching.
                        │  importer::run(request, cancel, emit) on a thread
src/importer.rs      The engine: walk, dedupe, name, copy/move, sidecars, dry
                     run, summary. Emits Event values over mpsc.
src/metadata.rs      Date resolution: EXIF → XMP sidecar → QuickTime header →
                     file times.
src/settings.rs      key=value persistence, validation of stored values.
```

Data flow of a run:

1. The user presses Start (button, `Ctrl+Enter`, or plain `Enter`).
2. `ImporterWindow::begin` calls `ImporterApp::start`, which validates, resets
   the counters, opens the run log file, spawns the worker, and calls
   `watch_run` (a GPUI timer, ~50 ms) which loops `drain_events`.
3. The worker calls `importer::run`, which sends `Event`s through an mpsc
   channel and polls an `Arc<AtomicBool>` cancel flag between files.
4. `ImporterApp::handle_event` updates counters, the log pane text, the day
   table and the run log file. `render` only paints that state.
5. `Event::Finished` / `Aborted` / `Fatal` end the run; `finish()` writes the
   summary tables and closes the log file.

`render` must stay free of per-frame work. Cached fields (`blocked_hint`,
`destination_is_dir`, `log_file_there`, `warnings`, `newest_log_file`,
`free_space`) are recomputed by `refresh_derived`/`refresh_warnings`/
`refresh_free_space` when something *changes*, not while painting. Keep it that
way: a `Path::exists()` or a directory walk inside `render` is a bug.

---

## 5. Module reference

### `src/main.rs` (40 lines)

- `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`.
- Loads `Settings`, computes window bounds with `usable_window_size()` /
  `usable_window_position()`, opens a `WindowOptions` with a custom titlebar
  title ("Image Importer") and `WindowBounds::Maximized`/`Windowed`.
- Calls `Theme::sync_system_appearance` once before building the view so the
  first frame matches the OS light/dark setting (the window keeps following it
  live, see §7).

### `src/settings.rs` (~850 lines incl. tests)

`Settings` is a plain struct of owned values, saved as `key=value` lines.
Constants: `DEFAULT_WINDOW_SIZE = (780.0, 700.0)`, `MIN_WINDOW_SIZE =
(620.0, 520.0)`, `MAX_WINDOW_EXTENT = 20_000.0` (stale placement cut-off),
`MAX_RECENT = 8`, `DEFAULT_LOGS_TO_KEEP = 5`, `MIN_LOGS_TO_KEEP = 1`,
`MAX_LOGS_TO_KEEP = 999`.

Keys written (only when non-default, so an ordinary file stays tiny):

| Key | Meaning |
| --- | --- |
| `source` | main source folder |
| `source_extra` (repeatable) | queued extra source folders, in order |
| `source_extra_off` (repeatable) | queued folders that are paused |
| `pinned` (repeatable) | favourite folders, first in the recent lists |
| `destination` | destination folder |
| `mode=move` | written only for Move; missing means Copy |
| `dry_run=1` | Dry run box ticked |
| `logs_to_keep=N` | run logs kept in the destination |
| `refuse_overlap=1` | refuse a run whose destination overlaps a source |
| `log_editor=` | program used by "Newest log" |
| `window_size=WxH` | logical window size |
| `window_position=X,Y` | logical window origin |
| `maximized=1` | reopen maximized |
| `recent_source=` / `recent_destination=` (repeatable) | recent lists, newest first |

Rules: unknown/malformed lines are ignored; unparsable booleans are false;
`logs_to_keep` is clamped; sizes below `MIN_WINDOW_SIZE` or positions beyond
`MAX_WINDOW_EXTENT` are treated as absent so a removed monitor cannot strand the
window; `cleared_of_folders()` (the Reset button) forgets folders, recents and
mode but keeps pins, window placement, `dry_run`, `refuse_overlap` and
`log_editor`.

`Settings::save()` deliberately swallows IO errors — remembering folders must
never interrupt an import.

### `src/metadata.rs` (~560 lines)

`shooting_time(path) -> Taken { when: NaiveDateTime, origin: DateOrigin }`.
Precedence:

1. EXIF `DateTimeOriginal`.
2. EXIF `DateTimeDigitized`, then IFD0 `DateTime`.
3. XMP sidecar beside the photo (`photo.xmp`, `photo.RAW.xmp`):
   `exif:DateTimeOriginal`, `xmp:CreateDate`, `photoshop:DateCreated`,
   `tiff:DateTime`.
4. Windows file creation time, then modification time.
5. Current local time (so nothing is ever skipped for want of a date).

For RAW containers the EXIF reader cannot open (`cr3`, `raf`, some TIFF
variants) the code locates the embedded TIFF header (`find_tiff_header`) and
parses that. Videos (`mov`, `mp4`, `m4v`, `3gp`, `mts`, `m2ts`, and the rest of
the extension list) get their date from the QuickTime `mvhd` creation time,
which is UTC and is therefore converted to local time — unlike EXIF, which is
used verbatim as wall-clock time. `plausible()` rejects firmware "unknown"
values such as `0000:00:00 00:00:00`, and `parse_exif_datetime` drops time
zones and fractional seconds.

`DateOrigin` is what the log prints in brackets (`[EXIF shooting date]`,
`[file created]`, …); its `describe()` is user-visible text.

### `src/importer.rs` (~830 lines)

Public surface: `is_supported_image`, `is_supported_video`, `is_importable`,
`is_sidecar`, `ImportRequest`, `TransferMode`, `Event`, `Summary`,
`SidecarMove`, `run(request, cancel, emit) -> Result<Summary, String>`.

Behaviour that must be preserved:

- **Walk order**: sources in the order given; within a source, `walkdir`
  entries sorted by name. Each source's own files are visited before moving on,
  so `Summary::per_source*` map to `ImportRequest::sources`.
- **Never feed on itself**: the destination subtree is skipped while walking a
  source; a source that sits inside the destination is skipped entirely and
  reported with `Event::SkippedSource`.
- **Duplicates**: if the target path exists and is byte-identical
  (`same_contents`, chunked compare), emit `Duplicate` and copy nothing. If it
  exists with different bytes, keep both, adding `_1`, `_2`, … before the
  extension (`resolve_target`). Nothing is ever overwritten.
- **Sidecars**: `.xmp`, `.aae`, `.pp3`, `.dop`, matched case-insensitively as
  either `NAME.ext.xmp` or `NAME.xmp`, within the photo's own folder. A sidecar
  is carried when its photo is imported, or when the photo is already in the
  library and the sidecar is missing there (`sidecar_plan`). Sidecars follow a
  renamed photo (`IMG_0001_1.CR3.xmp`). A sidecar found elsewhere in the tree
  whose photo has none is reported in `Summary::stray_sidecars` and *not*
  copied.
- **Move**: copy first, then delete, with a copy-then-delete fallback across
  volumes. If the copy lands but deletion fails, emit `Event::LeftBehind` and
  still count the file as imported (it is in the library).
- **Dry run**: no folder or file is created (`fs::create_dir_all` is skipped and
  the destination path is made absolute instead of canonicalized); paths are
  reported through `Event::Planned` and counted in `Summary::imported`.
- **Cancel**: polled between files only, so a run stops after the file in flight.
- **Copy preserves** the source's modified and accessed timestamps.
- Extensions are matched by name only; the file's contents are copied verbatim
  and never decoded for pixels.

### `src/app.rs` (~3.9k lines, more than half of it `#[cfg(test)]`)

`ImporterApp` is the model: field-per-UI-fact, no GPUI types at all, so it can
be unit-tested without a window. This is the file to extend for behaviour.

Notable members:

- `start()` — validation, log file creation, worker spawn, `watch_run`.
- `handle_event()` — the single place `Event` variants become UI state and log
  lines.
- `finish()` — summary tables (day, year, per-source, date span, paused note),
  written both to the pane and the file; stores `last_summary`.
- Validation/derivation helpers: `source_folders()`, `all_paused_error()`,
  `missing_sources()`, `overlap_pairs()`, `overlap_refusal()`,
  `start_blocked()`, `current_warnings()`, `low_space_warning()`,
  `photos_bytes()`, `free_bytes()`.
- Log file helpers: `create_run_log()`, `prune_old_logs()`, `delete_run_logs()`,
  `newest_run_log()`, `is_run_log()`.
- Process launching: `open_in_editor()`, `open_log_in()`, `open_in_notepad()`,
  `reveal_in_explorer()`, `open_folder()`, `select_arg()`.
- Text helpers: `size_label()`, `duration_label()`, `day_summary()`,
  `year_summary()`, `date_span()`, `summary_text()`, `live_day_summary()`,
  `live_year_summary()`, `counters_label()`, `bytes_label()`, `time_left()`.
- Input tidying: `clean_folder()` (strips quotes/spaces, turns a photo path into
  its folder), `merged_recents()`, `forget_recent()`, `move_item()`,
  `queue_position()`, `dropped_folders()`.
- `drain_events()`, `log_text()`, `summary_text()` consume the channel and make
  clipboard text.

`ImporterApp` also owns every user-visible string. Changing wording is a
behaviour change: tests assert on many of these sentences, and `README.md`
quotes them. Search for the sentence before editing.

### `src/ui_gpui.rs` (~1.5k lines)

One window, built from GPUI Kit components (`Button`, `Checkbox`, `TextInput`,
`Tooltip`, theme tokens). Structure:

`ImporterWindow { controller: ImporterApp, source_input, destination_input,
newest_log_label, _subscriptions, pump, last_bounds_save, fitted_to_content }`.

Sections, in paint order: `header`, `source_section` (field, recents dropdown,
pin, queue with pause/move/remove), `destination_section`, `options_section`
(mode, dry run, refuse overlap, logs kept, editor), `toolbar` (start/cancel,
open/show/copy log, open destination, reset, delete logs), `progress_section`
(bar, status, counters, bytes, time left, live day/year tables),
`summary_panels`, `log_panel`.

Key points for anyone modifying the view:

- No per-frame work in `render`: it calls `sync_inputs` (only when a path
  changed), `tick_free_space` (rate-limited internally) and `note_window`
  (rate-limited to every few hundred ms).
- Actions live in `bind_keys` and `CONTEXT` (`"ImageImporter"`); handlers are
  `cx.listener(Self::…)` methods on `ImporterWindow` that call straight through
  to `ImporterApp`. Adding a shortcut means: a `KeyBinding` in `bind_keys`, an
  action struct, a listener, and (usually) a dim hint line under the buttons.
- Theme: `Theme::sync_system_appearance` at window open (in `main.rs`) plus
  `cx.observe_window_appearance(...)` at construction, so the window follows the
  OS light/dark switch live. All colours come from `cx.theme()` tokens; do not
  hard-code colours.
- Window sizing: the window body measures its own children once
  (`on_children_prepainted`), and if the content is taller than the window it
  grew into, it resizes the window once — `needed = max(child.bottom) +
  24px` (the `p_6` bottom padding), capped at the display's visible height,
  skipped when maximized or already tall enough, guarded by
  `fitted_to_content: Rc<Cell<bool>>`. The body keeps `overflow_y_scroll` only
  as a safety net for a window shrunk by hand; at open nothing scrolls.
  `note_window` saves size/position/maximized state, and only while not running.
- The log pane is a fixed `h_64` (256 px) scrolling box; the day table is
  `max_h_64` with its own scroll. Long runs scroll inside those boxes by design.

### `tests/import_test.rs` (~1k lines)

Integration tests over the real engine using `tempfile` directories and
synthetic files: `build_tiff`, `build_jpeg` (wraps a TIFF in an APP1 segment),
`build_mov` (writes an `mvhd` atom), plus helpers `import()`, `import_sources()`
and `plan()`. Use these instead of checking in sample media. Unit tests for the
model live in the `#[cfg(test)] mod tests` block at the bottom of
`src/app.rs`; settings tests are at the bottom of `src/settings.rs`.

Test conventions: names are full sentences describing the guarantee
(`rerunning_the_same_import_skips_duplicates`), assertions prefer exact strings
for user-visible text, and filesystem tests always build their own temp tree.

---

## 6. Import rules cheat-sheet (what a change must not break)

1. Destination is never overwritten; identical files are skipped, different
   files get `_1` suffixes.
2. A run can never import its own output (destination subtree skipped; a source
   inside the destination is skipped and named in the log).
3. Dry runs create nothing at all — no destination folder, no log file.
4. The app only ever deletes: (a) source files in Move mode after a successful
   copy, (b) its own `import-log-*.txt` files beyond the keep count, or via
   **Delete logs**. Nothing else. Log pruning is restricted to
   `import-log-*.txt`.
5. Cancel is checked between files, never mid-file.
6. Every file that is touched gets a log line with size and resolved date, and
   the run log file receives the same lines as the pane.
7. A failed copy is never reported as success, and a successful copy whose
   original could not be deleted is reported as `LeftBehind`, counted imported.
8. Duplicate detection is byte comparison, not timestamps or size alone.
9. Sidecars stay with their photo, including after renames, and never cross
   folders.
10. Settings and log files are plain text, ASCII, one `key=value` or one log
    line at a time; no JSON, no databases.

---

## 7. Working on the code: recipes

**Add a file extension** — add it to `IMAGE_EXTENSIONS`, `RAW_EXTENSIONS` or
`VIDEO_EXTENSIONS` in `src/importer.rs`; extend the table in `README.md`; add a
test asserting `is_importable` (see `a_webm_video_counts_as_importable`).

**Change how a date is chosen** — edit the precedence chain in
`src/metadata.rs::shooting_time` and the `DateOrigin` variant it returns
(`describe()` is printed in the log). Add an integration test that builds the
container type you care about.

**Add a setting** — field on `Settings` + default, a key in `save_to`/`load_from`
(write only when non-default), a line in `README.md`'s settings block, and a
round-trip test in `src/settings.rs`. Decide explicitly whether
`cleared_of_folders()` keeps it (safety switches are kept; folder memory is
not).

**Add a UI control** — put state on `ImporterApp` (with its rule/validation as a
pure function so it can be unit-tested), call it from a handler in
`ui_gpui.rs`, paint it in the matching section with theme tokens. If it is a
safety switch, remember it in `settings.rs` and keep it through Reset.

**Add a keyboard shortcut** — action struct + `KeyBinding` in `bind_keys`, a
listener method, and the hint line in the toolbar (which must omit keys whose
button is currently hidden). Shortcuts must be ignored while a path field is
focused, except Ctrl+Enter and Escape (see `shortcut_action` and the guard in
`render`). `esc`/`Enter` routing has tests in `src/app.rs`.

**Change window sizing** — `DEFAULT_WINDOW_SIZE`/`MIN_WINDOW_SIZE` in
`settings.rs` and the fit logic in `ui_gpui.rs::render`. Windows are logical
points; on this machine 150 % scaling turns 1 logical point into 1.5 physical
pixels.

**Touch the log wording** — grep the sentence in `src/app.rs` and
`tests/import_test.rs` first; many strings are asserted exactly, and
`README.md` documents them.

---

## 8. Constraints, conventions and gotchas

- GPUI Kit is pinned at `0.9.0` and GPUI at `gpui-pre 0.3.11`. Check the
  vendored sources under `~/.cargo/registry/src/…` before assuming an API
  exists; `on_children_prepainted`, `Window::resize`, `Window::is_maximized`
  and `Window::display` are used, but many other gpui APIs are not.
- `Div::on_children_prepainted` exists on `Div`, **not** on `Stateful<Div>` —
  call it before `.id(...)`.
- GPUI wants `impl IntoElement` returns; shared UI helpers take
  `cx: &mut Context<Self>` and return `impl IntoElement`.
- The controller (`app.rs`) must stay GPUI-free. That separation is what makes
  ~100 model tests possible without a window.
- No new dependencies unless genuinely needed; prefer `std` and the existing
  crates. Anything Windows-specific (`open_in_explorer`, `open_folder`,
  `free_bytes`) goes through `std::process::Command` / `fs`.
- User-visible text is sentence-cased English with Windows path `\` style and
  an em dash where the README shows one; keep that voice.
- Comments explain *why* a rule exists (see the module docs and the doc
  comments throughout `app.rs`); match that style, and avoid comments that
  merely restate the code.
- The repository has no CI. "Verified" means: `cargo build --release` clean and
  `cargo test` green, plus a manual launch when the change is visual.

---

## 9. Known limits (documented, not bugs)

- Windows only in practice: settings live in `%APPDATA%`, drives are measured
  with Windows APIs, Explorer/Notepad integration is Windows-specific.
- Videos are dated from the QuickTime header of `mov`/`mp4`-family files; other
  containers fall back to their file times.
- Photos are never decoded for pixels, so a corrupt JPEG still imports.
- The log pane keeps at most `MAX_LOG_CHARS = 200_000` characters of the current
  run; the log file on disk is complete.
- No thumbnails, no preview, no duplicate search across the library beyond the
  target path, no undo of a finished Move run.
