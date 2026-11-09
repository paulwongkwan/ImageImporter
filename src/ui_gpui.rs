//! The GPUI Kit desktop window.
//!
//! This is the rebuilt interface: one window, the same workflow (source folders
//! in, photos filed by date out), drawn with GPUI Kit components and design
//! tokens instead of the immediate-mode egui layout. All import work, settings
//! and log handling stay in [`crate::app::ImporterApp`]; this module only
//! presents it and translates input into calls on it.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, Selectable as _, Theme,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState},
    progress::Progress,
};
use gpui_kit::*;
use gpui_kit::base::{h_flex, v_flex};

use crate::app::{
    clean_folder, differs, forget_recent, merged_recents, missing_folder_note, open_folder,
    queue_position, DropTarget, ImporterApp,
};
use crate::importer::TransferMode;
use crate::settings::{MAX_LOGS_TO_KEEP, MIN_LOGS_TO_KEEP};

actions!(image_importer, [
    StartRun,
    CancelRun,
    OpenRunLog,
    ShowRunLog,
    NewestRunLog,
    CopyRunSummary,
    CopyRunLog,
]);

/// Key context of the window's root element.
pub const CONTEXT: &str = "ImageImporter";

/// Keys that reach the window without going through a button.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", StartRun, Some(CONTEXT)),
        KeyBinding::new("escape", CancelRun, Some(CONTEXT)),
        KeyBinding::new("ctrl-l", OpenRunLog, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-l", ShowRunLog, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-n", NewestRunLog, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-c", CopyRunSummary, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-k", CopyRunLog, Some(CONTEXT)),
    ]);
}

/// The window: the controller plus the retained input state the fields need.
pub struct ImporterWindow {
    controller: ImporterApp,
    source_input: Entity<InputState>,
    destination_input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
    pump: Option<Task<()>>,
    last_bounds_save: Option<Instant>,
    /// Whether the window has already been grown once to fit its content.
    fitted_to_content: Rc<Cell<bool>>,
}

impl ImporterWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut controller = ImporterApp::default();
        controller.refresh_derived();

        let source_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Type, paste (Ctrl+V) or drop a folder")
                .default_value(controller.source.clone())
        });
        let destination_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Type, paste (Ctrl+V) or drop a folder")
                .default_value(controller.destination.clone())
        });

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &source_input,
            window,
            |this: &mut Self,
                state: &Entity<InputState>,
                event: &InputEvent,
                window: &mut Window,
                cx: &mut Context<Self>| match event {
                InputEvent::Change => {
                    this.controller.source = state.read(cx).value().to_string();
                    this.paths_changed(cx);
                }
                InputEvent::PressEnter { .. } => {
                    let text = state.read(cx).value().to_string();
                    this.set_source_text(&text, window, cx);
                    this.controller.remember_typed(false);
                    this.controller.save_settings();
                    cx.notify();
                }
                _ => {}
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &destination_input,
            window,
            |this: &mut Self,
                state: &Entity<InputState>,
                event: &InputEvent,
                window: &mut Window,
                cx: &mut Context<Self>| match event {
                InputEvent::Change => {
                    this.controller.destination = state.read(cx).value().to_string();
                    this.paths_changed(cx);
                }
                InputEvent::PressEnter { .. } => {
                    let text = state.read(cx).value().to_string();
                    this.set_destination_text(&text, window, cx);
                    this.controller.remember_typed(true);
                    this.controller.save_settings();
                    cx.notify();
                }
                _ => {}
            },
        ));

        Self {
            controller,
            source_input,
            destination_input,
            fitted_to_content: Rc::new(Cell::new(false)),
            _subscriptions: {
                // Windows can be switched between light and dark while the window
                // is open; follow it straight away instead of waiting for a restart.
                subscriptions.push(cx.observe_window_appearance(window, |_, window, cx| {
                    Theme::sync_system_appearance(Some(window), cx);
                }));
                subscriptions
            },
            pump: None,
            last_bounds_save: None,
        }
    }

    /// A path field changed by hand: the warnings and the free-space reading
    /// both described the old path.
    fn paths_changed(&mut self, cx: &mut Context<Self>) {
        self.controller.warnings_stale = false;
        self.controller.warnings.clear();
        self.controller.last_free_space_check = None;
        self.controller.refresh_derived();
        self.controller.refresh_free_space();
        self.controller.refresh_warnings();
        self.controller.save_settings();
        cx.notify();
    }

    fn set_source_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = clean_folder(text);
        self.controller.source = text.clone();
        self.source_input.update(cx, |state, cx| {
            state.set_value(text, window, cx);
        });
        self.paths_changed(cx);
    }

    fn set_destination_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = clean_folder(text);
        self.controller.destination = text.clone();
        self.destination_input.update(cx, |state, cx| {
            state.set_value(text, window, cx);
        });
        self.paths_changed(cx);
    }

    /// Pull the fields back in line with the controller, for changes that did not
    /// come from typing (a drop, a picker, a reset). Setting the value does not
    /// emit a change event, so this cannot loop.
    fn sync_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.controller.source.clone();
        if self.source_input.read(cx).value().as_ref() != source {
            self.source_input.update(cx, |state, cx| {
                state.set_value(source, window, cx);
            });
        }

        let destination = self.controller.destination.clone();
        if self.destination_input.read(cx).value().as_ref() != destination {
            self.destination_input.update(cx, |state, cx| {
                state.set_value(destination, window, cx);
            });
        }
    }

    /// Keep the remembered window placement in step with the real window.
    fn note_window(&mut self, window: &Window) {
        if self.controller.running {
            return;
        }

        let bounds = window.bounds();
        let width: f32 = bounds.size.width.into();
        let height: f32 = bounds.size.height.into();
        let left: f32 = bounds.origin.x.into();
        let top: f32 = bounds.origin.y.into();
        let size = (width, height);
        let position = (left, top);
        let moved = self
            .controller
            .window_size
            .is_none_or(|current| differs(current, size))
            || self
                .controller
                .window_position
                .is_none_or(|current| differs(current, position));
        if !moved {
            return;
        }

        self.controller.window_size = Some(size);
        self.controller.window_position = Some(position);

        let due = self
            .last_bounds_save
            .is_none_or(|last| last.elapsed() >= Duration::from_millis(500));
        if due {
            self.last_bounds_save = Some(Instant::now());
            self.controller.save_settings();
        }
    }

    // ---- commands ----------------------------------------------------------

    fn start_run(&mut self, _: &StartRun, _: &mut Window, cx: &mut Context<Self>) {
        self.begin(cx);
    }

    fn cancel_run(&mut self, _: &CancelRun, _: &mut Window, cx: &mut Context<Self>) {
        self.controller.cancel_import();
        cx.notify();
    }

    fn open_run_log(&mut self, _: &OpenRunLog, _: &mut Window, cx: &mut Context<Self>) {
        self.controller.open_log();
        cx.notify();
    }

    fn show_run_log(&mut self, _: &ShowRunLog, _: &mut Window, cx: &mut Context<Self>) {
        self.controller.show_log();
        cx.notify();
    }

    fn newest_run_log(&mut self, _: &NewestRunLog, _: &mut Window, cx: &mut Context<Self>) {
        self.controller.newest_log();
        cx.notify();
    }

    fn copy_run_summary(
        &mut self,
        _: &CopyRunSummary,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy_summary_now(cx);
    }

    fn copy_run_log(&mut self, _: &CopyRunLog, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_log_now(cx);
    }

    fn copy_summary_clicked(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_summary_now(cx);
    }

    fn copy_log_clicked(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_log_now(cx);
    }

    fn copy_summary_now(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.controller.summary_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        cx.notify();
    }

    fn copy_log_now(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.controller.log_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        cx.notify();
    }

    fn begin(&mut self, cx: &mut Context<Self>) {
        if !self.controller.can_start() || self.controller.start_blocked().is_some() {
            self.controller.start();
            cx.notify();
            return;
        }

        self.controller.start();
        if self.controller.running {
            self.watch_run(cx);
        }
        cx.notify();
    }

    /// Poll the worker's channel while it runs, so the window keeps up with the
    /// events without a repaint loop when nothing is happening.
    fn watch_run(&mut self, cx: &mut Context<Self>) {
        self.pump = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;

                let running = this.update(cx, |this, cx| {
                    let running = this.controller.drain_events();
                    cx.notify();
                    running
                });

                if !matches!(running, Ok(true)) {
                    break;
                }
            }
        }));
    }

    fn browse_source(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = PathBuf::from(clean_folder(&self.controller.source));
        let mut dialog = rfd::FileDialog::new();
        if current.is_dir() {
            dialog = dialog.set_directory(&current);
        }
        if let Some(folder) = dialog.pick_folder() {
            self.set_source_text(&folder.display().to_string(), window, cx);
        }
    }

    fn browse_destination(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = PathBuf::from(clean_folder(&self.controller.destination));
        let mut dialog = rfd::FileDialog::new();
        if current.is_dir() {
            dialog = dialog.set_directory(&current);
        }
        if let Some(folder) = dialog.pick_folder() {
            self.set_destination_text(&folder.display().to_string(), window, cx);
        }
    }

    fn pin_source(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let folder = self.controller.source.trim().to_owned();
        if folder.is_empty() {
            return;
        }
        let on = !self.controller.is_pinned(&folder);
        self.controller.set_pinned(&folder, on);
        self.controller.save_settings();
        cx.notify();
    }

    fn pin_destination(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let folder = self.controller.destination.trim().to_owned();
        if folder.is_empty() {
            return;
        }
        let on = !self.controller.is_pinned(&folder);
        self.controller.set_pinned(&folder, on);
        self.controller.save_settings();
        cx.notify();
    }

    fn create_source(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let folder = clean_folder(&self.controller.source);
        self.controller.create_missing_folder(&folder);
        self.controller.refresh_derived();
        cx.notify();
    }

    fn create_destination(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let folder = clean_folder(&self.controller.destination);
        self.controller.create_missing_folder(&folder);
        self.controller.refresh_derived();
        cx.notify();
    }

    fn pick_recent_source(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let recent = merged_recents(
            &self.controller.recent_sources,
            &self.controller.recent_sources,
            &self.controller.pinned,
        );
        if let Some(folder) = recent.get(index).cloned() {
            self.set_source_text(&folder, window, cx);
        }
    }

    fn pick_recent_destination(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let recent = merged_recents(
            &self.controller.recent_destinations,
            &self.controller.recent_sources,
            &self.controller.pinned,
        );
        if let Some(folder) = recent.get(index).cloned() {
            self.set_destination_text(&folder, window, cx);
        }
    }

    fn forget_recent_source(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let folder = self.controller.source.trim().to_owned();
        forget_recent(
            &mut self.controller.recent_sources,
            &mut self.controller.pinned,
            &folder,
        );
        self.controller.save_settings();
        cx.notify();
    }

    fn add_sources(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(folders) = rfd::FileDialog::new()
            .set_title("Choose folders to add to the queue")
            .pick_folders()
        else {
            return;
        };

        let (added, skipped) = self
            .controller
            .add_sources(folders.into_iter().map(|f| f.display().to_string()));

        self.controller.status = if added == 0 {
            "Nothing new to add: that folder is already in the list.".to_owned()
        } else {
            let word = if added == 1 { "folder" } else { "folders" };
            match skipped {
                0 => format!("Added {added} source {word}."),
                skipped => format!("Added {added} source {word}; {skipped} already in the list."),
            }
        };

        if added > 0 {
            self.source_input.update(cx, |state, cx| {
                state.set_value(self.controller.source.clone(), window, cx);
            });
            self.controller.save_settings();
        }
        self.controller.refresh_derived();
        cx.notify();
    }

    fn remove_queued(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.controller.extra_sources.len() {
            return;
        }
        let folder = self.controller.extra_sources.remove(index);
        self.controller
            .disabled_sources
            .retain(|entry| !entry.eq_ignore_ascii_case(folder.trim()));
        self.controller.save_settings();
        self.controller.refresh_derived();
        cx.notify();
    }

    fn toggle_paused(&mut self, index: usize, paused: bool, cx: &mut Context<Self>) {
        if let Some(folder) = self.controller.extra_sources.get(index).cloned() {
            self.controller.set_disabled(&folder, paused);
            self.controller.save_settings();
            self.controller.refresh_derived();
            cx.notify();
        }
    }

    fn move_queued(&mut self, index: usize, up: bool, cx: &mut Context<Self>) {
        let list = &mut self.controller.extra_sources;
        if up && index > 0 {
            list.swap(index, index - 1);
        } else if !up && index + 1 < list.len() {
            list.swap(index, index + 1);
        } else {
            return;
        }
        self.controller.save_settings();
        self.controller.refresh_derived();
        cx.notify();
    }

    fn unpause_all(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.controller.unpause_all();
        self.controller.save_settings();
        self.controller.refresh_derived();
        cx.notify();
    }

    fn set_mode(&mut self, mode: TransferMode, cx: &mut Context<Self>) {
        self.controller.mode = mode;
        self.controller.save_settings();
        cx.notify();
    }



    fn open_destination(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let destination = PathBuf::from(clean_folder(&self.controller.destination));
        if let Err(error) = open_folder(&destination) {
            self.controller.error = Some(format!(
                "Could not open {}: {error}",
                destination.display()
            ));
        }
        cx.notify();
    }

    fn choose_editor(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let chosen = rfd::FileDialog::new()
            .add_filter("Programs", &["exe"])
            .set_title("Choose the program that opens run logs")
            .pick_file();
        if let Some(program) = chosen {
            self.controller.log_editor = program.display().to_string();
            self.controller.save_settings();
        }
        cx.notify();
    }

    fn keep_more(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.controller
            .set_log_limit(self.controller.kept_logs().saturating_add(1).min(MAX_LOGS_TO_KEEP));
        self.controller.refresh_derived();
        cx.notify();
    }

    fn keep_fewer(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.controller
            .set_log_limit(self.controller.kept_logs().saturating_sub(1).max(MIN_LOGS_TO_KEEP));
        self.controller.refresh_derived();
        cx.notify();
    }

    fn delete_logs(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.controller.delete_logs();
        cx.notify();
    }

    fn reset_memory(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.controller.clear_memory();
        crate::settings::Settings::clear();
        let source = self.controller.source.clone();
        let destination = self.controller.destination.clone();
        self.source_input.update(cx, |state, cx| {
            state.set_value(source, window, cx);
        });        self.destination_input.update(cx, |state, cx| {
            state.set_value(destination, window, cx);
        });
        self.controller.refresh_derived();
        cx.notify();
    }


    // ---- rendering ---------------------------------------------------------

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(div().text_xl().child("Image Importer"))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "Files photos and videos into <destination>/<year>/<year-month-day>/ \
                         using the date stored in each file.",
                    ),
            )
    }

    fn section_title(&self, cx: &mut Context<Self>, text: &str) -> impl IntoElement {
        div()
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().muted_foreground)
            .child(text.to_owned())
    }

    fn source_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.controller.running;
        let folder = self.controller.source.trim().to_owned();
        let pinned = self.controller.is_pinned(&folder);
        let recent = merged_recents(
            &self.controller.recent_sources,
            &self.controller.recent_sources,
            &self.controller.pinned,
        );

        v_flex()
            .gap_2()
            .child(self.section_title(cx, "Source folders"))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().w_32().child("Source folder"))
                    .child({
                        let this = cx.entity();
                        div()
                            .flex_1()
                            .min_w_0()
                            .on_drop::<ExternalPaths>(
                                move |paths: &ExternalPaths, _window, cx: &mut App| {
                                    let dropped = paths.paths().to_vec();
                                    this.update(cx, |this, cx| {
                                        this.controller
                                            .handle_dropped_folder(dropped, DropTarget::Source);
                                        this.controller.refresh_derived();
                                        this.controller.refresh_warnings();
                                        cx.notify();
                                    });
                                },
                            )
                            .child(Input::new(&self.source_input).disabled(running))
                    })
                    .child(
                        Button::new("browse-source")
                            .label("Browse…")
                            .disabled(running)
                            .on_click(cx.listener(Self::browse_source)),
                    )
                    .child(
                        Button::new("pin-source")
                            .ghost()
                            .label(if pinned { "★" } else { "☆" })
                            .tooltip(if pinned {
                                "Unpin this folder"
                            } else {
                                "Pin this folder to the top of the recent list"
                            })
                            .disabled(running || folder.is_empty())
                            .on_click(cx.listener(Self::pin_source)),
                    ),
            )
            .children(self.note_lines(
                &self.controller.source,
                false,
                "create-source",
                cx,
                Self::create_source,
            ))
            .child(self.recents_row(&recent, false, cx))
            .child(self.queue_section(cx))
    }

    fn destination_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.controller.running;
        let folder = self.controller.destination.trim().to_owned();
        let pinned = self.controller.is_pinned(&folder);
        let recent = merged_recents(
            &self.controller.recent_destinations,
            &self.controller.recent_sources,
            &self.controller.pinned,
        );

        v_flex()
            .gap_2()
            .child(self.section_title(cx, "Destination"))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().w_32().child("Destination"))
                    .child({
                        let this = cx.entity();
                        div()
                            .flex_1()
                            .min_w_0()
                            .on_drop::<ExternalPaths>(
                                move |paths: &ExternalPaths, _window, cx: &mut App| {
                                    let dropped = paths.paths().to_vec();
                                    this.update(cx, |this, cx| {
                                        this.controller.handle_dropped_folder(
                                            dropped,
                                            DropTarget::Destination,
                                        );
                                        this.controller.refresh_derived();
                                        this.controller.refresh_warnings();
                                        cx.notify();
                                    });
                                },
                            )
                            .child(Input::new(&self.destination_input).disabled(running))
                    })
                    .child(
                        Button::new("browse-destination")
                            .label("Browse…")
                            .disabled(running)
                            .on_click(cx.listener(Self::browse_destination)),
                    )
                    .child(
                        Button::new("pin-destination")
                            .ghost()
                            .label(if pinned { "★" } else { "☆" })
                            .tooltip(if pinned {
                                "Unpin this folder"
                            } else {
                                "Pin this folder to the top of the recent list"
                            })
                            .disabled(running || folder.is_empty())
                            .on_click(cx.listener(Self::pin_destination)),
                    ),
            )
            .children(self.note_lines(
                &self.controller.destination,
                true,
                "create-destination",
                cx,
                Self::create_destination,
            ))
            .child(self.recents_row(&recent, true, cx))
    }

    /// The folder-is-missing note, clickable so the folder can be made here.
    fn note_lines(
        &self,
        value: &str,
        may_be_created: bool,
        id: &'static str,
        cx: &mut Context<Self>,
        create: fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>),
    ) -> Vec<AnyElement> {
        let mut lines = Vec::new();

        if let Some(note) = missing_folder_note(value, may_be_created) {
            lines.push(
                h_flex()
                    .gap_2()
                    .child(div().w_32())
                    .child(
                        Button::new(id)
                            .ghost()
                            .small()
                            .label(format!("⚠ {note} — create it"))
                            .tooltip("Create this folder now")
                            .on_click(cx.listener(create)),
                    )
                    .into_any_element(),
            );
        }

        for warning in &self.controller.warnings {
            lines.push(
                h_flex()
                    .gap_2()
                    .child(div().w_32())
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().warning)
                            .child(format!("⚠ {warning}")),
                    )
                    .into_any_element(),
            );
        }

        if let Some(label) = self.controller.free_space_label() {
            lines.push(
                h_flex()
                    .gap_2()
                    .child(div().w_32())
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(label),
                    )
                    .into_any_element(),
            );
        }

        lines
    }

    fn recents_row(
        &self,
        recent: &[String],
        destination: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let existing: Vec<(usize, String)> = recent
            .iter()
            .enumerate()
            .filter(|(_, path)| std::path::Path::new(path).is_dir())
            .map(|(index, path)| (index, path.clone()))
            .collect();

        let mut row = h_flex().gap_1().items_center().flex_wrap().child(div().w_32());

        if existing.is_empty() {
            row = row.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("No folders used yet."),
            );
            return row;
        }

        row = row.child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Recent"),
        );

        for (index, path) in existing {
            let name = if self.controller.is_pinned(&path) {
                format!("★ {path}")
            } else {
                path
            };
            let handler = if destination {
                Self::pick_recent_destination
            } else {
                Self::pick_recent_source
            };
            row = row.child(
                Button::new((
                    if destination {
                        "recent-destination"
                    } else {
                        "recent-source"
                    },
                    index,
                ))
                    .ghost()
                    .small()
                    .label(name)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        handler(this, index, window, cx)
                    })),
            );
        }

        row.child(
            Button::new(("forget-recent", destination as usize))
                .ghost()
                .small()
                .label("Forget current")
                .tooltip("Remove the folder in the field from the recent list")
                .disabled(self.controller.source.trim().is_empty())
                .on_click(cx.listener(Self::forget_recent_source)),
        )
    }

    fn queue_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.controller.running;
        let count = self.controller.source_folders().len();
        let paused = self.controller.paused_folders().len();

        let mut section = v_flex().gap_1();

        let mut actions = h_flex().gap_2().items_center().child(div().w_32());
        actions = actions
            .child(
                Button::new("add-sources")
                    .label("Add folder…")
                    .disabled(running)
                    .on_click(cx.listener(Self::add_sources)),
            )
            .child(
                Button::new("unpause-all")
                    .ghost()
                    .label("Unpause all")
                    .disabled(running || paused == 0)
                    .on_click(cx.listener(Self::unpause_all)),
            );

        if count > 1 || paused > 0 {
            let mut label = format!("{count} source folders");
            if paused > 0 {
                label.push_str(&format!(", {paused} paused"));
            }
            actions = actions.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            );
        }

        section = section.child(actions);

        for (index, folder) in self.controller.extra_sources.iter().enumerate() {
            let paused_here = self.controller.is_disabled(folder);
            let mut name = folder.clone();
            if paused_here {
                name.push_str("   (paused)");
            }

            section = section.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().w_32())
                    .child(
                        div()
                            .w_8()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{}.", queue_position(index))),
                    )
                    .child({
                        let this = cx.entity();
                        Checkbox::new(("queued", index))
                            .checked(!paused_here)
                            .label("")
                            .tooltip("Untick to skip this folder without removing it")
                            .disabled(running)
                            .on_click(move |checked: &bool, _window, cx: &mut App| {
                                let paused = !checked;
                                this.update(cx, |this, cx| {
                                    this.toggle_paused(index, paused, cx);
                                });
                            })
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(if paused_here {
                                cx.theme().muted_foreground
                            } else {
                                cx.theme().foreground
                            })
                            .child(name),
                    )
                    .child(
                        Button::new(("queued-up", index))
                            .ghost()
                            .small()
                            .label("↑")
                            .tooltip("Move up in the import order")
                            .disabled(running || index == 0)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.move_queued(index, true, cx);
                            })),
                    )
                    .child(
                        Button::new(("queued-down", index))
                            .ghost()
                            .small()
                            .label("↓")
                            .tooltip("Move down in the import order")
                            .disabled(
                                running || index + 1 >= self.controller.extra_sources.len(),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.move_queued(index, false, cx);
                            })),
                    )
                    .child(
                        Button::new(("queued-remove", index))
                            .ghost()
                            .small()
                            .label("✕")
                            .tooltip("Remove this folder from the queue")
                            .disabled(running)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_queued(index, cx);
                            })),
                    ),
            );
        }

        section
    }

    fn options_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.controller.running;
        let copy = self.controller.mode == TransferMode::Copy;

        v_flex()
            .gap_2()
            .child(self.section_title(cx, "Options"))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().w_32().child("Mode"))
                    .child(
                        Button::new("mode-copy")
                            .label("Copy (keep originals)")
                            .selected(copy)
                            .disabled(running)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_mode(TransferMode::Copy, cx);
                            })),
                    )
                    .child(
                        Button::new("mode-move")
                            .label("Move (delete originals after copying)")
                            .selected(!copy)
                            .disabled(running)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_mode(TransferMode::Move, cx);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().w_32())
                    .child({
                        let this = cx.entity();
                        Checkbox::new("dry-run")
                            .checked(self.controller.dry_run)
                            .label("Dry run — list planned destinations, copy nothing")
                            .disabled(running)
                            .on_click(move |checked: &bool, _window, cx: &mut App| {
                                let checked = *checked;
                                this.update(cx, |this, cx| {
                                    this.controller.dry_run = checked;
                                    this.controller.save_settings();
                                    cx.notify();
                                });
                            })
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().w_32())
                    .child({
                        let this = cx.entity();
                        Checkbox::new("refuse-overlap")
                            .checked(self.controller.refuse_overlap)
                            .label("Refuse to start if the destination overlaps a source folder")
                            .tooltip(
                                "Off by default: the clash is only warned about. Tick this and Start \
                                 refuses a destination that is a source, inside one, or above one.",
                            )
                            .disabled(running)
                            .on_click(move |checked: &bool, _window, cx: &mut App| {
                                let checked = *checked;
                                this.update(cx, |this, cx| {
                                    this.controller.refuse_overlap = checked;
                                    this.controller.save_settings();
                                    cx.notify();
                                });
                            })
                    }),
            )
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let blocked = self.controller.blocked_hint.clone();
        let can_start = self.controller.can_start() && blocked.is_none();
        let log_ready = self.controller.log_ready();
        let editor = self.controller.editor_name();
        // Worked out when the folders or the run changed, never while painting.
        let newest = if self.controller.running {
            None
        } else {
            self.controller.newest_log_file.clone()
        };
        let start_label = if self.controller.dry_run {
            "Preview plan"
        } else {
            "Start import"
        };

        let mut row = h_flex().gap_2().items_center().child(div().w_32())
            .child(
                Button::new("start")
                    .primary()
                    .label(start_label)
                    .disabled(!can_start)
                    .tooltip(
                        blocked
                            .clone()
                            .unwrap_or_else(|| "Enter, or Ctrl+Enter".to_owned()),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.begin(cx))),
            )
            .child(
                Button::new("cancel")
                    .label("Cancel")
                    .disabled(!self.controller.running)
                    .tooltip("Escape")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.cancel_import();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("open-destination")
                    .label("Open folder")
                    .tooltip("Open the destination in File Explorer")
                    .disabled(!self.controller.destination_is_dir)
                    .on_click(cx.listener(Self::open_destination)),
            );

        if log_ready {
            row = row
                .child(
                    Button::new("open-log")
                        .ghost()
                        .label("Open log")
                        .tooltip("Open this run's log file (Ctrl+L)")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller.open_log();
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("show-log")
                        .ghost()
                        .label("Show log")
                        .tooltip("Show this run's log in Explorer (Ctrl+Shift+L)")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller.show_log();
                            cx.notify();
                        })),
                );
        }

        if newest.is_some() {
            row = row.child(
                Button::new("newest-log")
                    .ghost()
                    .label(format!("Newest log in {editor}"))
                    .tooltip("Open the latest run log (Ctrl+Shift+N)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.newest_log();
                        cx.notify();
                    })),
            );
        }

        if self.controller.last_summary.is_some() {
            row = row.child(
                Button::new("copy-summary")
                    .ghost()
                    .label("Copy summary")
                    .tooltip("Copy this run's summary (Ctrl+Shift+C)")
                    .on_click(cx.listener(Self::copy_summary_clicked)),
            );
        }

        if log_ready {
            row = row.child(
                Button::new("copy-log")
                    .ghost()
                    .label("Copy log")
                    .tooltip("Copy this run's whole log (Ctrl+Shift+K)")
                    .on_click(cx.listener(Self::copy_log_clicked)),
            );
        }

        v_flex()
            .gap_2()
            .child(row)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().w_32())
                    .child(
                        Button::new("choose-editor")
                            .ghost()
                            .small()
                            .label(format!("Editor: {editor}"))
                            .tooltip("Choose the program Newest log opens")
                            .on_click(cx.listener(Self::choose_editor)),
                    )
                    .child(div().w_px().h_4().bg(cx.theme().border))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Keep"),
                    )
                    .child(
                        Button::new("keep-fewer")
                            .ghost()
                            .small()
                            .label("−")
                            .disabled(self.controller.running)
                            .on_click(cx.listener(Self::keep_fewer)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .w_5()
                            .text_center()
                            .child(self.controller.kept_logs().to_string()),
                    )
                    .child(
                        Button::new("keep-more")
                            .ghost()
                            .small()
                            .label("+")
                            .disabled(self.controller.running)
                            .on_click(cx.listener(Self::keep_more)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("logs"),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("delete-logs")
                            .ghost()
                            .small()
                            .label("Delete logs")
                            .tooltip("Delete every run log in the destination")
                            .disabled(
                                self.controller.running
                                    || self.controller.destination.trim().is_empty(),
                            )
                            .on_click(cx.listener(Self::delete_logs)),
                    )
                    .child(
                        Button::new("reset")
                            .ghost()
                            .small()
                            .label("Reset")
                            .tooltip("Forget the remembered folders and mode")
                            .disabled(self.controller.running)
                            .on_click(cx.listener(Self::reset_memory)),
                    ),
            )
    }

    fn progress_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let fraction = if self.controller.total > 0 {
            (self.controller.done as f32 / self.controller.total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let progress_text = if self.controller.total > 0 {
            format!(
                "{}/{} files",
                self.controller.done.min(self.controller.total),
                self.controller.total
            )
        } else {
            String::new()
        };

        let mut section = v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().w_32())
                    .child(
                        div().flex_1().min_w_0().child(
                            Progress::new("run-progress")
                                .value(fraction * 100.0)
                                .loading(self.controller.running && self.controller.total == 0),
                        ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(progress_text),
                    )
                    .children(self.controller.time_left().map(|left| {
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(left)
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w_32())
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.controller.counters_label()),
                    ),
            );

        if let Some(label) = self.controller.bytes_label() {
            section = section.child(
                h_flex().gap_2().child(div().w_32()).child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                ),
            );
        }

        section = section.child(
            h_flex().gap_2().child(div().w_32()).child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.controller.status.clone()),
            ),
        );

        if let Some(error) = &self.controller.error {
            section = section.child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(div().w_32())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(format!("⚠ {error}")),
                    ),
            );
        }

        section
    }

    fn summary_panels(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut panels = Vec::new();

        if let Some(table) = self.controller.live_day_summary() {
            panels.push(
                div()
                    .id("day-table")
                    .pl_32()
                    .max_h_64()
                    .overflow_y_scroll()
                    .text_sm()
                    .font_family("Consolas")
                    .text_color(cx.theme().muted_foreground)
                    .child(table)
                    .into_any_element(),
            );
        }

        if let Some(line) = self.controller.live_year_summary() {
            panels.push(
                div()
                    .pl_32()
                    .text_sm()
                    .font_family("Consolas")
                    .text_color(cx.theme().muted_foreground)
                    .child(line)
                    .into_any_element(),
            );
        }

        panels
    }

    fn log_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let text = if self.controller.log.is_empty() {
            "Nothing imported yet.".to_owned()
        } else {
            self.controller.log.clone()
        };

        v_flex()
            .gap_2()
            .child(self.section_title(cx, "Log"))
            .child(
                div()
                    .id("log-body")
                    .h_64()
                    .min_h_0()
                    .overflow_y_scroll()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .p_2()
                    .child(
                        div()
                            .text_sm()
                            .font_family("Consolas")
                            .child(text),
                    ),
            )
    }
}

impl Render for ImporterWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_inputs(window, cx);
        self.controller.tick_free_space();
        self.note_window(window);

        let fitted_to_content = self.fitted_to_content.clone();

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::start_run))
            .on_action(cx.listener(Self::cancel_run))
            .on_action(cx.listener(Self::open_run_log))
            .on_action(cx.listener(Self::show_run_log))
            .on_action(cx.listener(Self::newest_run_log))
            .on_action(cx.listener(Self::copy_run_summary))
            .on_action(cx.listener(Self::copy_run_log))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    // The whole window is sized to the content, so the body itself
                    // never scrolls. If the content is taller than the window it
                    // opened with, grow the window once instead of scrolling.
                    .on_children_prepainted({
                        let fitted = fitted_to_content.clone();
                        move |children: Vec<Bounds<Pixels>>, window: &mut Window, cx: &mut App| {
                            if fitted.replace(true) || children.is_empty() {
                                return;
                            }

                            let Some(bottom) = children.iter().map(|child| child.bottom()).max()
                            else {
                                return;
                            };

                            // `p_6` leaves the same padding below the last section.
                            let needed = bottom + px(24.0);
                            let viewport = window.viewport_size();
                            if window.is_maximized() || needed <= viewport.height + px(1.0) {
                                return;
                            }

                            let screen = window
                                .display(cx)
                                .map(|display| display.visible_bounds().size.height);
                            let height = screen.map_or(needed, |limit| needed.min(limit));
                            window.resize(size(viewport.width, height));
                        }
                    })
                    .id("window-body")
                    // A safety net only: the window opens tall enough that the body
                    // never needs scrolling, but shrinking it by hand still works.
                    .overflow_y_scroll()
                    .p_6()
                    .gap_4()
                    .child(self.header(cx))
                    .child(self.source_section(cx))
                    .child(self.destination_section(cx))
                    .child(self.options_section(cx))
                    .child(self.toolbar(cx))
                    .child(self.progress_section(cx))
                    .children(self.summary_panels(cx))
                    .child(self.log_panel(cx)),
            )
    }
}
