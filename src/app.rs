//! The window's state: folders, settings, the run worker and its log.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use chrono::Local;

use crate::importer::{self, Event, ImportRequest, SidecarMove, Summary, TransferMode};
use crate::settings::Settings;

use crate::settings::{DEFAULT_LOGS_TO_KEEP, MAX_LOGS_TO_KEEP, MIN_LOGS_TO_KEEP};

/// Keep the visible log bounded; older lines are dropped from the front.
const MAX_LOG_CHARS: usize = 200_000;

/// Application state, all owned by the main (UI) thread.
pub struct ImporterApp {
    pub(crate) source: String,
    /// Extra source folders queued for the same run.
    pub(crate) extra_sources: Vec<String>,
    /// Queued folders that are paused: shown, remembered, but not imported.
    pub(crate) disabled_sources: Vec<String>,
    pub(crate) destination: String,
    /// Folders used recently for each field, most recent first.
    pub(crate) recent_sources: Vec<String>,
    pub(crate) recent_destinations: Vec<String>,
    /// Folders pinned as favourites: first in the recent lists, never dropped.
    pub(crate) pinned: Vec<String>,
    /// Warnings raised by the current folders (overlap, free space).
    pub(crate) warnings: Vec<String>,
    /// Set by a drag and drop, which changes the fields without them reporting
    /// an edit, so the warnings know to look again.
    pub(crate) warnings_stale: bool,
    /// The run log a click on "Newest log" would open, as last looked at.
    /// Scanning the destination belongs to a change, not to a repaint.
    pub(crate) newest_log_file: Option<PathBuf>,
    /// Why Start is greyed out, as last worked out from the folders.
    pub(crate) blocked_hint: Option<String>,
    /// Whether the destination folder exists, as last looked at.
    pub(crate) destination_is_dir: bool,
    /// Whether this run's log file is on disk, as last looked at.
    pub(crate) log_file_there: bool,
    /// When the free space was last measured, so typing does not walk the
    /// sources on every keystroke.
    pub(crate) last_space_check: Option<Instant>,
    /// Free bytes on the destination drive, shown under the folder fields.
    pub(crate) free_space: Option<u64>,
    /// When that reading was taken, so it refreshes on a timer while a run goes.
    pub(crate) last_free_space_check: Option<Instant>,
    /// The last finished run's summary, offered to the clipboard.
    pub(crate) last_summary: Option<String>,
    pub(crate) mode: TransferMode,
    /// How many run logs the destination keeps.
    pub(crate) logs_to_keep: usize,
    pub(crate) dry_run: bool,
    /// Refuse to start when the destination is one of the source folders.
    pub(crate) refuse_overlap: bool,
    /// Program that opens run logs; empty means Notepad.
    pub(crate) log_editor: String,
    /// Bytes this run has written so far, for the running total under the bar.
    pub(crate) bytes: u64,
    pub(crate) running: bool,
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) events: Option<Receiver<Event>>,
    pub(crate) source_root: PathBuf,
    pub(crate) destination_root: PathBuf,
    pub(crate) total: usize,
    pub(crate) done: usize,
    pub(crate) imported: usize,
    pub(crate) duplicates: usize,
    pub(crate) failed: usize,
    /// How many edit sidecars travelled with their photos this run.
    pub(crate) sidecars: usize,
    /// How many photos landed in each `year-month-day` folder this run.
    pub(crate) day_counts: BTreeMap<String, usize>,
    pub(crate) status: String,
    pub(crate) log: String,
    /// Receives the same lines as the log pane while a run is active.
    pub(crate) log_file: Option<File>,
    /// Log file written by the current (or most recent) run.
    pub(crate) log_path: Option<PathBuf>,
    pub(crate) error: Option<String>,
    /// Screen areas of the two path rows, used to aim a dropped folder.
    /// Window placement as last reported by the window itself.
    pub(crate) window_size: Option<(f32, f32)>,
    pub(crate) window_position: Option<(f32, f32)>,
    /// Whether the window was maximized as last reported by the window.
    pub(crate) maximized: bool,
    /// When the running import started, for the time-remaining estimate.
    pub(crate) started_at: Option<Instant>,
}

impl Default for ImporterApp {
    fn default() -> Self {
        let saved = Settings::load();
        let status = startup_status(saved.dry_run);

        Self {
            source: saved.source,
            extra_sources: saved.extra_sources,
            disabled_sources: saved.disabled_sources,
            destination: saved.destination,
            recent_sources: saved.recent_sources,
            recent_destinations: saved.recent_destinations,
            pinned: saved.pinned,
            warnings: Vec::new(),
            warnings_stale: false,
            newest_log_file: None,
            blocked_hint: None,
            destination_is_dir: false,
            log_file_there: false,
            last_space_check: None,
            free_space: None,
            last_free_space_check: None,
            last_summary: None,
            mode: saved.mode,
            logs_to_keep: saved.logs_to_keep,
            dry_run: saved.dry_run,
            refuse_overlap: saved.refuse_overlap,
            log_editor: saved.log_editor.clone(),
            bytes: 0,
            running: false,
            cancel: Arc::new(AtomicBool::new(false)),
            events: None,
            source_root: PathBuf::new(),
            destination_root: PathBuf::new(),
            total: 0,
            done: 0,
            imported: 0,
            sidecars: 0,
            duplicates: 0,
            failed: 0,
            day_counts: BTreeMap::new(),
            status: status.to_owned(),
            log: String::new(),
            log_file: None,
            log_path: None,
            error: None,
            window_size: saved.window_size,
            window_position: saved.window_position,
            maximized: saved.maximized,
            started_at: None,
        }
    }
}


impl ImporterApp {

    pub(crate) fn start(&mut self) {
        self.error = None;
        self.total = 0;
        self.done = 0;
        self.imported = 0;
        self.duplicates = 0;
        self.failed = 0;
        self.sidecars = 0;
        self.bytes = 0;
        self.day_counts.clear();
        self.last_summary = None;
        self.log.clear();

        let sources: Vec<PathBuf> = self.source_folders().into_iter().map(PathBuf::from).collect();
        let destination = PathBuf::from(clean_folder(&self.destination));

        if sources.is_empty() {
            self.error = Some(
                self.all_paused_error()
                    .unwrap_or_else(|| "Pick at least one source folder.".to_owned()),
            );
            return;
        }
        let listed: Vec<String> = sources
            .iter()
            .map(|folder| folder.display().to_string())
            .collect();

        // A folder that is not on disk has nothing to read from it, whether it was
        // mistyped, deleted or unplugged. Every missing folder is named, so a
        // queue with two bad entries does not need two runs to find that out.
        if let Some(clash) = missing_source_clash(&listed) {
            self.error = Some(format!(
                "Nothing started: {clash}, so there is nothing to read. Fix the path, plug the drive back in, or add a folder that exists."
            ));
            return;
        }
        // With the safety ticked, a destination that overlaps a source in any way
        // - the same folder, or one nested inside the other - is refused outright
        // rather than only warned about.
        if self.refuse_overlap {
            if let Some(refusal) = overlap_refusal(&listed, &self.destination) {
                self.error = Some(refusal);
                return;
            }
        }

        self.source_root = sources[0].clone();
        self.destination_root = destination.clone();

        self.warnings = self.current_warnings();
        for warning in self.warnings.clone() {
            self.push_log(format!("! {warning}"));
        }

        // Folders earn a place in the dropdowns by actually being imported from
        // or into, so the lists stay meaningful.
        Settings::remember_with(&mut self.recent_sources, &self.source, &self.pinned);
        Settings::remember_with(&mut self.recent_destinations, &self.destination, &self.pinned);
        self.save_settings();

        // A dry run promises to touch nothing in the destination, so it keeps
        // its log in the window only.
        self.close_log_file();
        self.log_path = None;
        if !self.dry_run {
            let stamp = Local::now().format("%Y-%m-%d_%H%M%S").to_string();
            match create_run_log(&destination, &stamp, self.kept_logs()) {
                Ok((file, path)) => {
                    self.log_file = Some(file);
                    self.write_log_header(&sources[0], &destination);
                    self.push_log(format!("Log file: {}", path.display()));
                    self.log_path = Some(path);
                }
                Err(error) => self.push_log(format!(
                    "! could not create a log file in the destination: {error}"
                )),
            }
        }

        let (sender, receiver) = channel::<Event>();
        self.events = Some(receiver);
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&self.cancel);
        let mode = self.mode;
        let dry_run = self.dry_run;

        self.started_at = Some(Instant::now());
        self.running = true;
        self.status = "Starting…".to_owned();

        std::thread::spawn(move || {
            let request = ImportRequest {
                sources,
                destination,
                mode,
                dry_run,
            };

            let result = {
                let mut emit = |event: Event| {
                    let _ = sender.send(event);
                };
                importer::run(&request, &cancel, &mut emit)
            };

            if let Err(error) = result {
                let _ = sender.send(Event::Fatal(error));
            }
        });
    }


    pub(crate) fn handle_event(&mut self, event: Event) {
        match event {
            Event::SkippedSource { folder } => {
                self.push_log(format!(
                    "! Skipped {}: it is inside the destination, so its files are already in the library.",
                    folder.display()
                ));
            }
            Event::Scanning => {
                self.status = "Scanning the source folder…".to_owned();
                self.push_log("Scanning the source folder…");
            }
            Event::Discovered(total) => {
                self.total = total;
                if total == 0 {
                    self.status = "No supported image files found.".to_owned();
                    self.push_log("No supported image files found in the source folder.");
                } else {
                    self.status = format!("Found {total} image files. Importing…");
                    self.push_log(format!("Found {total} image files."));
                }
            }
            Event::Imported {
                index,
                total,
                from,
                to,
                origin,
                size,
                taken,
                sidecars,
            } => {
                self.done = index;
                self.imported += 1;
                self.sidecars += sidecars.len();
                self.bytes += size;
                record_day(&mut self.day_counts, &to);
                let shown = relative(&self.destination_root, &to);
                self.status = format!("{index}/{total}  →  {shown}");
                self.push_log(format!(
                    "+ {}  →  {}   {}   {}   [{}]",
                    relative(&self.source_root, &from),
                    shown,
                    size_label(size),
                    taken,
                    origin.describe()
                ));
                self.log_sidecars(&sidecars, "[sidecar]");
            }
            Event::Planned {
                index,
                total,
                from,
                to,
                origin,
                size,
                taken,
                sidecars,
            } => {
                self.done = index;
                self.imported += 1;
                self.sidecars += sidecars.len();
                self.bytes += size;
                record_day(&mut self.day_counts, &to);
                let shown = relative(&self.destination_root, &to);
                self.status = format!("{index}/{total}  would go to  {shown}");
                self.push_log(format!(
                    "~ {}  →  {}   {}   {}   [{}]",
                    relative(&self.source_root, &from),
                    shown,
                    size_label(size),
                    taken,
                    origin.describe()
                ));
                self.log_sidecars(&sidecars, "[sidecar]");
            }
            Event::Duplicate {
                index,
                total,
                from,
                to,
                size,
                taken,
                sidecars,
            } => {
                self.done = index;
                self.duplicates += 1;
                self.sidecars += sidecars.len();
                self.status = format!("{index}/{total}  duplicate, skipped");
                self.push_log(format!(
                    "= {}  →  {}   {}   {}   [already there, skipped]",
                    relative(&self.source_root, &from),
                    relative(&self.destination_root, &to),
                    size_label(size),
                    taken
                ));
                self.log_sidecars(&sidecars, "[sidecar added]");
            }
            Event::Failed {
                index,
                total,
                from,
                size,
                taken,
                reason,
            } => {
                self.done = index;
                self.failed += 1;
                self.status = format!("{index}/{total}  failed");
                self.push_log(format!(
                    "! {}   {}   {}   [{reason}]",
                    relative(&self.source_root, &from),
                    size_label(size),
                    taken
                ));
            }
            Event::LeftBehind { from, to, reason } => {
                // The copy is in the library; only the original stayed put. Said
                // in a sentence, because "failed" would misdescribe the file.
                self.push_log(format!(
                    "! Copied {} to {}, but could not delete it from the source: {reason}",
                    relative(&self.source_root, &from),
                    relative(&self.destination_root, &to)
                ));
            }
            Event::Finished(summary) => {
                let verb = if self.dry_run { "Dry run" } else { "Done" };
                self.finish(&summary, verb);
            }
            Event::Aborted(summary) => self.finish(&summary, "Stopped"),
            Event::Fatal(message) => {
                self.running = false;
                self.events = None;
                self.status = "Import did not start.".to_owned();
                self.push_log(format!("! {message}"));
                self.error = Some(message);
                self.close_log_file();
            }
        }
    }

    /// Whether a run can be started right now.
    pub(crate) fn can_start(&self) -> bool {
        !self.running
            && !self.source.trim().is_empty()
            && !self.destination.trim().is_empty()
    }

    /// The reason Start is greyed out even though both paths are filled in: a
    /// source folder that is not on disk, or the refusal tick meeting an overlap.
    pub(crate) fn start_blocked(&self) -> Option<String> {
        let folders = self.source_folders();

        // A folder that is not there cannot be read, so it is said before the
        // overlap: the path itself is what has to be fixed.
        if let Some(clash) = missing_source_clash(&folders) {
            return Some(format!(
                "Greyed out: {clash}. Fix the path, plug the drive back in, or add a folder that exists."
            ));
        }

        if !self.refuse_overlap {
            return None;
        }

        overlap_refusal_hint(&folders, &self.destination)
    }

    /// Ask the running import to stop after the file being copied.
    pub(crate) fn cancel_import(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.status = "Stopping after the current file…".to_owned();
    }

    /// Whether this run's log can be opened or shown right now, as last looked
    /// at by [`Self::refresh_derived`], so painting does not stat the file.
    pub(crate) fn log_ready(&self) -> bool {
        !self.running && self.log_file_there
    }

    /// Whether this run's log file is actually on disk, for the actions that open
    /// or copy it. Painting asks [`Self::log_ready`] instead, which is cached, so
    /// no stat happens while the window is drawn.
    fn log_file_present(&self) -> bool {
        self.log_path.as_ref().is_some_and(|path| path.is_file())
    }

    /// Open this run's log in whatever program is registered for `.txt` files.
    pub(crate) fn open_log(&mut self) {
        if self.running || !self.log_file_present() {
            return;
        }

        if let Some(path) = self.log_path.clone() {
            if let Err(error) = open_in_editor(&path) {
                self.error = Some(format!("Could not open {}: {error}", path.display()));
            }
        }
    }

    /// Show this run's log in Explorer with the file selected.
    pub(crate) fn show_log(&mut self) {
        if self.running || !self.log_file_present() {
            return;
        }

        if let Some(path) = self.log_path.clone() {
            if let Err(error) = reveal_in_explorer(&path) {
                self.error = Some(format!("Could not show {}: {error}", path.display()));
            }
        }
    }

    /// The program Newest log opens, as named in the toolbar: Notepad unless one
    /// has been chosen.
    pub(crate) fn editor_name(&self) -> String {
        let chosen = self.log_editor.trim();
        if chosen.is_empty() {
            return "Notepad".to_owned();
        }

        Path::new(chosen)
            .file_stem()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| chosen.to_owned())
    }

    /// The chosen editor, or `None` for Notepad.
    pub(crate) fn editor(&self) -> Option<PathBuf> {
        let chosen = self.log_editor.trim();
        if chosen.is_empty() {
            return None;
        }

        Some(PathBuf::from(chosen))
    }

    /// Open the newest log in the destination in the chosen program.
    pub(crate) fn newest_log(&mut self) {
        if self.running {
            return;
        }

        let Some(path) = newest_run_log(&PathBuf::from(clean_folder(&self.destination))) else {
            return;
        };
        if let Err(error) = open_log_in(self.editor().as_deref(), &path) {
            self.error = Some(format!("Could not open {}: {error}", path.display()));
        }
    }




    /// The running total of bytes written, shown under the counters.
    pub(crate) fn bytes_label(&self) -> Option<String> {
        if self.bytes == 0 {
            return None;
        }

        let verb = if self.dry_run { "planned" } else { "copied" };
        Some(format!("{} {verb} so far", size_label(self.bytes)))
    }

    /// The one-line tally shown under the progress bar.
    pub(crate) fn counters_label(&self) -> String {
        let imported = if self.dry_run { "Planned" } else { "Imported" };

        format!(
            "{imported} {}   ·   duplicates {}   ·   failed {}   ·   sidecars {}",
            self.imported, self.duplicates, self.failed, self.sidecars
        )
    }

    /// How much longer the running import is likely to take, if it can be told.
    pub(crate) fn time_left(&self) -> Option<String> {
        if !self.running {
            return None;
        }

        let started = self.started_at?;
        estimate_left(self.done, self.total, started.elapsed())
    }

    /// The per-day table as it stands, shown while a run is in progress.
    pub(crate) fn live_day_summary(&self) -> Option<String> {
        if !self.running || self.day_counts.is_empty() {
            return None;
        }

        let heading = if self.dry_run {
            "Files per day so far (planned):"
        } else {
            "Files per day so far:"
        };

        Some(format!("{heading}\n{}", day_summary(&self.day_counts)))
    }

    /// The per-year roll-up as it stands, shown under the live day table.
    pub(crate) fn live_year_summary(&self) -> Option<String> {
        if !self.running || self.day_counts.is_empty() {
            return None;
        }

        let heading = if self.dry_run {
            "Files per year so far (planned):"
        } else {
            "Files per year so far:"
        };

        Some(format!(
            "{heading}  {}",
            year_summary(&self.day_counts)
        ))
    }

    pub(crate) fn finish(&mut self, summary: &Summary, verb: &str) {
        self.running = false;
        self.events = None;
        // The run's log is now there to reopen, so the button can appear.
        self.refresh_derived();
        self.total = summary.total;
        self.done = summary.total;
        self.imported = summary.imported;
        self.duplicates = summary.duplicates;
        self.failed = summary.failed;

        // The engine's own tally is authoritative, so the counters row matches
        // the message even if an event was missed.
        self.imported = summary.imported;
        self.duplicates = summary.duplicates;
        self.failed = summary.failed;
        self.sidecars = summary.sidecars;
        // The engine's tally includes sidecars, so the running total ends up
        // matching the message rather than only the photos it saw.
        self.bytes = summary.bytes;

        let seconds = summary.elapsed.as_secs_f32();
        let sidecars = if summary.sidecars > 0 {
            format!(", {} sidecars", summary.sidecars)
        } else {
            String::new()
        };
        let size = if summary.bytes > 0 {
            format!(" ({})", size_label(summary.bytes))
        } else {
            String::new()
        };
        let message = if self.dry_run {
            format!(
                "{verb}: {} files would be imported{size}{sidecars}, {} duplicates skipped, {} failed in {seconds:.1}s",
                summary.imported, summary.duplicates, summary.failed
            )
        } else {
            format!(
                "{verb}: {} imported{size}{sidecars}, {} duplicates skipped, {} failed in {seconds:.1}s",
                summary.imported, summary.duplicates, summary.failed
            )
        };
        self.status = message.clone();
        self.push_log(message);

        // A paused folder is a deliberate choice, but the log should still say
        // which folders the run left out - and how much went with them.
        let paused = self.paused_folders();
        let paused_note = match paused.len() {
            0 => None,
            _ => {
                let named: Vec<String> = paused
                    .iter()
                    .map(|folder| {
                        // A folder that has gone since it was queued has nothing
                        // left to count, so it is named without a figure.
                        if !Path::new(folder).is_dir() {
                            return folder.clone();
                        }

                        let count = folder_file_count(Path::new(folder));
                        let word = if count == 1 { "file" } else { "files" };
                        format!("{folder} ({count} {word})")
                    })
                    .collect();

                Some(if paused.len() == 1 {
                    format!(
                        "Skipped 1 source folder because it was paused: {}.",
                        named[0]
                    )
                } else {
                    format!(
                        "Skipped {} source folders because they were paused: {}.",
                        paused.len(),
                        named.join(", ")
                    )
                })
            }
        };
        if let Some(note) = &paused_note {
            self.push_log(note.clone());
        }

        let day_table = if self.day_counts.is_empty() {
            None
        } else {
            let heading = if self.dry_run {
                "Files per day folder (planned):"
            } else {
                "Files per day folder:"
            };
            Some(format!("{heading}\n{}", day_summary(&self.day_counts)))
        };
        if let Some(table) = &day_table {
            self.push_log(table.clone());
        }

        let year_line = if self.day_counts.is_empty() {
            None
        } else {
            Some(format!(
                "Files per year:  {}",
                year_summary(&self.day_counts)
            ))
        };
        if let Some(line) = &year_line {
            self.push_log(line.clone());
        }

        let span_line = date_span(&self.day_counts);
        if let Some(line) = &span_line {
            self.push_log(line.clone());
        }

        let source_table = if summary.per_source.len() > 1 {
            Some(format!(
                "Files per source folder:\n{}",
                source_summary(
                    &self.source_folders(),
                    &summary.per_source,
                    &summary.per_source_duplicates,
                    &summary.per_source_failed,
                )
            ))
        } else {
            None
        };
        if let Some(table) = &source_table {
            self.push_log(table.clone());
        }

        // The same text, ready for the clipboard: the totals line first, then the
        // tables that follow it in the log, headings included.
        let mut summary_text = self.status.clone();
        for table in [day_table, year_line, span_line, source_table, paused_note]
            .into_iter()
            .flatten()
        {
            summary_text.push('\n');
            summary_text.push_str(&table);
        }
        self.last_summary = Some(summary_text);

        if !summary.stray_sidecars.is_empty() {
            const SHOWN: usize = 10;
            let count = summary.stray_sidecars.len();
            self.push_log(format!(
                "Warning: {count} {} no sidecar in its own folder, but a matching one exists elsewhere:",
                if count == 1 { "photo has" } else { "photos have" }
            ));
            for (photo, elsewhere) in summary.stray_sidecars.iter().take(SHOWN) {
                self.push_log(format!(
                    "  {}   →   {}   [left behind: sidecars stay in their own folder]",
                    photo.display(),
                    elsewhere.display()
                ));
            }
            if count > SHOWN {
                self.push_log(format!("  … and {} more", count - SHOWN));
            }
        }

        self.close_log_file();
    }

    /// A folder dragged onto the window fills the row it was dropped on, and the
    /// source folder when it lands anywhere else.
    pub(crate) fn handle_dropped_folder(&mut self, paths: Vec<PathBuf>, target: DropTarget) {
        if self.running || paths.is_empty() {
            return;
        }

        // Dropping a photo means its folder, which is what dragging out of an
        // Explorer file list gives you. Several photos from different places
        // each bring their own folder.
        for path in paths.iter().filter(|path| dropped_folder(path).is_none()) {
            self.push_log(format!("! ignored dropped file {}", path.display()));
        }

        let folders = dropped_folders(&paths);
        let Some(first) = folders.first().cloned() else {
            let name = paths
                .first()
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            self.status = format!("Cannot tell which folder {name} belongs to.");
            return;
        };

        self.warnings_stale = true;
        let text = first.display().to_string();
        let note = match paths.as_slice() {
            [only] if only != &first => format!(" (folder of {})", only.display()),
            _ => String::new(),
        };

        match target {
            DropTarget::Source => {
                if self.source.trim().is_empty() {
                    self.source = text.clone();
                    self.status = format!("Source folder set to {text}{note}");
                    self.push_log(format!("Source folder set by drag and drop: {text}{note}"));
                } else if self.add_extra_source(&text) {
                    let count = self.source_folders().len();
                    self.status =
                        format!("Added to the source list: {text}{note} ({count} folders)");
                    self.push_log(format!(
                        "Source folder queued by drag and drop: {text}{note} ({count} folders)"
                    ));
                } else {
                    self.status = format!("{text} is already in the source list.");
                }

                // The rest of the drop follows the first folder into the queue,
                // so a multi-folder drop needs no second drag.
                let mut queued = 0;
                for folder in folders.iter().skip(1) {
                    if self.add_extra_source(&folder.display().to_string()) {
                        queued += 1;
                    }
                }
                if queued > 0 {
                    let count = self.source_folders().len();
                    let word = if queued == 1 { "folder" } else { "folders" };
                    self.status = format!(
                        "{queued} more source {word} queued from the drop ({count} folders)"
                    );
                    self.push_log(format!(
                        "Queued {queued} more source {word} from a drag and drop ({count} folders)"
                    ));
                }
            }
            DropTarget::Destination => {
                self.destination = text.clone();
                self.status = format!("Destination set to {text}{note}");
                self.push_log(format!("Destination set by drag and drop: {text}{note}"));
                if folders.len() > 1 {
                    let extra = folders.len() - 1;
                    self.push_log(format!(
                        "! ignored {extra} other dropped folder(s): the destination is a single folder"
                    ));
                }
            }
        }
        self.error = None;
        self.save_settings();
    }


    /// Describe the run at the top of its log file.
    pub(crate) fn write_log_header(&mut self, source: &Path, destination: &Path) {
        let mode = match self.mode {
            TransferMode::Copy => "copy",
            TransferMode::Move => "move",
        };

        if let Some(file) = &mut self.log_file {
            let _ = writeln!(file, "# Image Importer run log");
            let _ = writeln!(
                file,
                "# Started: {}",
                Local::now().format("%Y-%m-%d %H:%M:%S")
            );
            let _ = writeln!(file, "# Source: {}", source.display());
            let _ = writeln!(file, "# Destination: {}", destination.display());
            let _ = writeln!(file, "# Mode: {mode}");
            let _ = writeln!(file);
        }
    }

    /// Close the run log, if one is open.
    pub(crate) fn close_log_file(&mut self) {
        if let Some(mut file) = self.log_file.take() {
            let _ = file.flush();
        }
    }


    /// Queue another source folder, ignoring blanks and folders already listed.
    /// Returns whether the list changed.
    pub(crate) fn add_extra_source(&mut self, folder: &str) -> bool {
        let folder = folder.trim();
        if folder.is_empty() {
            return false;
        }
        if self
            .source_folders()
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(folder))
        {
            return false;
        }

        self.extra_sources.push(folder.to_owned());
        true
    }

    /// Whether a folder is pinned as a favourite.
    pub(crate) fn is_pinned(&self, folder: &str) -> bool {
        crate::settings::is_pinned(&self.pinned, folder)
    }

    /// Pin or unpin a favourite, lifting it to the front of both recent lists.
    pub(crate) fn set_pinned(&mut self, folder: &str, on: bool) {
        Settings::set_pinned(&mut self.pinned, folder, on);
        Settings::pinned_first(&mut self.recent_sources, &self.pinned);
        Settings::pinned_first(&mut self.recent_destinations, &self.pinned);
    }

    /// Whether a queued folder is paused for this run.
    pub(crate) fn is_disabled(&self, folder: &str) -> bool {
        let folder = folder.trim();
        self.disabled_sources
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(folder))
    }

    /// Resume every paused folder at once, for when the whole queue has been
    /// ticked off. Returns how many came back.
    pub(crate) fn unpause_all(&mut self) -> usize {
        let resumed = self.paused_folders().len();
        if resumed == 0 {
            return 0;
        }

        self.disabled_sources.clear();
        self.status = match resumed {
            1 => "Resumed 1 paused source folder.".to_owned(),
            count => format!("Resumed {count} paused source folders."),
        };

        resumed
    }

    /// Add folders to the queue, filling the source field first when it is empty
    /// so a fresh window that picks several is ready to run. Returns how many were
    /// added and how many were already in the list.
    pub(crate) fn add_sources<I>(&mut self, folders: I) -> (usize, usize)
    where
        I: IntoIterator<Item = String>,
    {
        let mut added = 0;
        let mut skipped = 0;

        for folder in folders {
            let folder = clean_folder(&folder);
            if folder.is_empty() {
                continue;
            }

            if self.source.trim().is_empty() {
                self.source = folder;
                added += 1;
                continue;
            }

            // A folder already in the field is not queued a second time.
            let same_as_field = path_key(&folder) == path_key(&self.source);
            if same_as_field || !self.add_extra_source(&folder) {
                skipped += 1;
            } else {
                added += 1;
            }
        }

        (added, skipped)
    }

    /// Pause or resume a queued folder, leaving it in its place in the list.
    pub(crate) fn set_disabled(&mut self, folder: &str, disabled: bool) {
        let folder = folder.trim();
        self.disabled_sources
            .retain(|entry| !entry.eq_ignore_ascii_case(folder));
        if disabled {
            self.disabled_sources.push(folder.to_owned());
        }
    }

    /// Measure the destination drive's free space. One Windows call and no
    /// walking, so it is cheap enough to repeat while a run is going.
    pub(crate) fn refresh_free_space(&mut self) {
        let destination = PathBuf::from(clean_folder(&self.destination));
        self.free_space = existing_ancestor(&destination).and_then(free_bytes);
    }

    /// Re-measure at most once a second, so the reading follows a running import
    /// without a syscall on every frame.
    pub(crate) fn tick_free_space(&mut self) {
        let due = self
            .last_free_space_check
            .is_none_or(|last| last.elapsed() >= Duration::from_millis(1000));
        if !due {
            return;
        }

        self.last_free_space_check = Some(Instant::now());
        self.refresh_free_space();
    }

    /// The free space reading, as shown under the folder fields.
    pub(crate) fn free_space_label(&self) -> Option<String> {
        let free = self.free_space?;
        let destination = PathBuf::from(clean_folder(&self.destination));
        let drive = destination
            .components()
            .next()
            .map(|prefix| prefix.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_else(|| "Destination".to_owned());

        Some(format!("{drive} {} free", size_label(free)))
    }

    /// Every warning the current folders deserve, worked out from scratch.
    pub(crate) fn current_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        let folders = self.source_folders();

        // A destination inside a source means the run copies into a tree it is
        // also reading. It is allowed, but worth saying out loud - once for every
        // source folder that clashes, not only the first one found.
        warnings.extend(destination_overlaps(&folders, &self.destination));

        // A dry run copies nothing, so free space cannot run out on one.
        if !self.dry_run {
            let sources: Vec<PathBuf> = folders.iter().map(PathBuf::from).collect();
            let destination = PathBuf::from(clean_folder(&self.destination));
            if let Some(warning) = low_space_warning(&sources, &destination) {
                warnings.push(warning);
            }
        }

        warnings
    }

    /// Whatever the window reads from the disk - the newest run log to reopen
    /// and the reason Start is greyed out - looked up again. Both cost a folder
    /// read or a stat per source, so they are refreshed when the folders, the
    /// queue or the run change, and never while the window is being painted.
    pub(crate) fn refresh_derived(&mut self) {
        let destination = PathBuf::from(clean_folder(&self.destination));
        self.blocked_hint = self.start_blocked();
        self.destination_is_dir = destination.is_dir();
        self.log_file_there =
            !self.running && self.log_path.as_ref().is_some_and(|path| path.is_file());
        self.newest_log_file = if self.running {
            None
        } else {
            newest_run_log(&destination)
        };
    }

    /// Look again as the folders are edited, so the space warning appears as soon
    /// as the destination is picked rather than only at Start. Measuring walks
    /// the sources, so it waits until the destination is a real folder and runs at
    /// most twice a second while a path is being typed.
    pub(crate) fn refresh_warnings(&mut self) {
        if self.running || !PathBuf::from(clean_folder(&self.destination)).is_dir() {
            return;
        }

        let now = Instant::now();
        if let Some(last) = self.last_space_check {
            if now.duration_since(last) < Duration::from_millis(500) {
                return;
            }
        }
        self.last_space_check = Some(now);

        self.warnings = self.current_warnings();
    }

    /// Why a run has nothing to do when pausing is the whole reason, so the
    /// message does not blame an empty folder field.
    pub(crate) fn all_paused_error(&self) -> Option<String> {
        let paused = self.paused_folders();
        if paused.is_empty()
            || paused.len() < self.extra_sources.len()
            || !clean_folder(&self.source).is_empty()
        {
            return None;
        }

        // Naming them saves scanning the list to work out which ticks to move.
        let names = paused.join(", ");
        Some(if paused.len() == 1 {
            format!(
                "Nothing to import: the only source folder is paused: {names}. Untick it in the list to include it."
            )
        } else {
            format!(
                "Nothing to import: all {} source folders are paused: {names}. Untick one in the list to include it.",
                paused.len()
            )
        })
    }

    /// Move the main source folder into the queue at `index`, promoting the
    /// first queued folder to the field so the run still starts with a folder.
    /// Returns whether anything moved.
    #[cfg(test)]
    pub(crate) fn demote_main_source(&mut self, index: usize) -> bool {
        let main = clean_folder(&self.source);
        if main.is_empty() || self.extra_sources.is_empty() || index == 0 {
            return false;
        }

        let mut list = self.extra_sources.clone();
        list.insert(index.min(list.len()), main);
        let promoted = list.remove(0);

        // The field is always imported, so a paused folder that moves into it is
        // switched back on.
        self.set_disabled(&promoted, false);

        self.source = promoted;
        self.extra_sources = list;
        true
    }

    /// The queued folders paused for this run, in queue order, so the end-of-run
    /// note can name them rather than only count them.
    pub(crate) fn paused_folders(&self) -> Vec<String> {
        self.extra_sources
            .iter()
            .filter(|folder| self.is_disabled(folder))
            .map(|folder| folder.trim().to_owned())
            .collect()
    }


    /// Every folder this run imports from: the field first, then the queue.
    pub(crate) fn source_folders(&self) -> Vec<String> {
        let mut folders: Vec<String> = Vec::new();

        let first = clean_folder(&self.source);
        if !first.is_empty() {
            folders.push(first);
        }

        for folder in &self.extra_sources {
            let folder = folder.trim();
            if folder.is_empty() || self.is_disabled(folder) {
                continue;
            }
            if folders.iter().any(|entry| entry.eq_ignore_ascii_case(folder)) {
                continue;
            }
            folders.push(folder.to_owned());
        }

        folders
    }


    /// Forget the remembered folders and mode. The stored settings file is
    /// removed by the caller, so this part stays pure and testable.
    pub(crate) fn clear_memory(&mut self) {
        self.source.clear();
        self.extra_sources.clear();
        self.disabled_sources.clear();
        self.destination.clear();
        self.recent_sources.clear();
        self.recent_destinations.clear();
        self.mode = TransferMode::Copy;
        self.error = None;
        self.status = "Remembered folders and mode cleared.".to_owned();
        self.push_log("Reset: remembered folders and mode cleared.");
    }

    /// How many run logs to keep, treating an unset value as the default.
    pub(crate) fn kept_logs(&self) -> usize {
        match self.logs_to_keep {
            0 => DEFAULT_LOGS_TO_KEEP,
            count => count.clamp(MIN_LOGS_TO_KEEP, MAX_LOGS_TO_KEEP),
        }
    }

    /// Take a newly chosen log count: remember it, and drop the surplus logs
    /// now rather than leaving them until the next run creates its own.
    pub(crate) fn set_log_limit(&mut self, keep: usize) {
        let keep = keep.clamp(MIN_LOGS_TO_KEEP, MAX_LOGS_TO_KEEP);
        if self.logs_to_keep == keep {
            return;
        }

        self.logs_to_keep = keep;
        self.save_settings();

        let destination = PathBuf::from(clean_folder(&self.destination));
        if !destination.is_dir() {
            self.status = format!("Keeping the newest {keep} run logs.");
            return;
        }

        let removed = prune_old_logs(&destination, keep);
        let note = if removed == 0 {
            format!("Logs: keeping the newest {keep}, no older ones to remove")
        } else {
            format!("Logs: keeping the newest {keep}, removed {removed} older")
        };

        self.status = if removed == 0 {
            format!("Keeping the newest {keep} run logs; none had to go.")
        } else {
            format!("Kept the newest {keep} run logs, deleted {removed} older.")
        };

        // The pane can be open on a log that was just pruned; it is older than
        // the newest few only if the count was lowered a long way, but a file
        // that is gone must not stay open and "Open log" must not point at it.
        if let Some(path) = &self.log_path {
            if !path.exists() {
                self.close_log_file();
                self.log_path = None;
            }
        }

        self.push_log(&note);
    }

    /// Delete every run log in the destination, leaving everything else alone.
    pub(crate) fn delete_logs(&mut self) {
        let destination = PathBuf::from(clean_folder(&self.destination));
        if !destination.is_dir() {
            self.error = Some("Pick a destination folder before deleting logs.".to_owned());
            return;
        }

        // The pane keeps its text, but a file that is about to go must not stay
        // open, and "Open log" should not point at it.
        self.close_log_file();
        self.log_path = None;

        match delete_run_logs(&destination) {
            Ok(0) => {
                self.status = "No run logs to delete.".to_owned();
                self.push_log(format!(
                    "Delete logs: none in {}",
                    destination.display()
                ));
            }
            Ok(count) => {
                self.status = format!("Deleted {count} run log(s) from the destination.");
                self.push_log(format!(
                    "Delete logs: removed {count} run log(s) from {}",
                    destination.display()
                ));
            }
            Err(error) => {
                let message = format!("Could not delete run logs: {error}");
                self.push_log(format!("! {message}"));
                self.error = Some(message);
            }
        }

        // Whatever survived is what "Newest log" may still open.
        self.refresh_derived();
    }

    /// Add the folder now in a field to its Recent list, tidied, so a folder
    /// typed once is offered again without needing a run. An empty field adds
    /// nothing.
    pub(crate) fn remember_typed(&mut self, destination: bool) {
        if destination {
            let folder = clean_folder(&self.destination);
            Settings::remember_with(&mut self.recent_destinations, &folder, &self.pinned);
        } else {
            let folder = clean_folder(&self.source);
            Settings::remember_with(&mut self.recent_sources, &folder, &self.pinned);
        }
    }

    /// Remember the current folders for the next launch.
    pub(crate) fn save_settings(&self) {
        Settings {
            source: clean_folder(&self.source),
            extra_sources: self.extra_sources.clone(),
            disabled_sources: self.disabled_sources.clone(),
            destination: clean_folder(&self.destination),
            mode: self.mode,
            logs_to_keep: self.kept_logs(),
            dry_run: self.dry_run,
            refuse_overlap: self.refuse_overlap,
            log_editor: self.log_editor.clone(),
            window_size: self.window_size,
            window_position: self.window_position,
            maximized: self.maximized,
            recent_sources: self.recent_sources.clone(),
            recent_destinations: self.recent_destinations.clone(),
            pinned: self.pinned.clone(),
        }
        .save();
    }

    /// Note the sidecars that travelled with a photo.
    pub(crate) fn log_sidecars(&mut self, sidecars: &[SidecarMove], note: &str) {
        for sidecar in sidecars {
            self.push_log(format!(
                "  ↳ {}  →  {}   {}   {note}",
                relative(&self.source_root, &sidecar.from),
                relative(&self.destination_root, &sidecar.to),
                size_label(sidecar.size)
            ));
        }
    }

    /// Make the folder a path field said was missing, so a click on the warning
    /// fixes the path without a trip to Explorer, then look again at the warnings
    /// and the free space.
    pub(crate) fn create_missing_folder(&mut self, folder: &str) {
        match fs::create_dir_all(folder) {
            Ok(()) => {
                self.status = format!("Created {folder}");
                self.push_log(format!("Created the missing folder {folder}"));
                self.error = None;
            }
            Err(error) => {
                self.error = Some(format!("Could not create {folder}: {error}"));
            }
        }

        // The note and the space reading are both out of date now.
        self.warnings_stale = true;
        self.last_free_space_check = None;
        self.refresh_free_space();
    }

    pub(crate) fn push_log(&mut self, line: impl AsRef<str>) {
        let line = line.as_ref();

        if !self.log.is_empty() {
            self.log.push('\n');
        }
        self.log.push_str(line);

        // The run file keeps every line, even after the pane drops old ones.
        if let Some(file) = &mut self.log_file {
            let _ = writeln!(file, "{line}");
        }

        if self.log.len() > MAX_LOG_CHARS {
            let mut cut = self.log.len() - MAX_LOG_CHARS / 2;
            while cut < self.log.len() && !self.log.is_char_boundary(cut) {
                cut += 1;
            }
            if let Some(newline) = self.log[cut..].find('\n') {
                self.log.drain(..cut + newline + 1);
            }
        }
    }
}

/// The note to show under a path field whose folder is not there. A destination
/// is allowed not to exist - the first run creates it - so only a path with
/// nothing above it on disk is worth warning about there.
pub(crate) fn missing_folder_note(value: &str, may_be_created: bool) -> Option<String> {
    let folder = clean_folder(value);
    if folder.is_empty() {
        return None;
    }

    let path = Path::new(&folder);
    if path.is_dir() {
        return None;
    }
    if may_be_created && existing_ancestor(path).is_some_and(|found| found.is_dir()) {
        return None;
    }

    Some(format!("No folder at {folder}"))
}



/// Create a fresh log file for this run inside the destination folder.
///
/// The timestamped name keeps runs apart; a run that starts in the same second
/// gets a `_2`, `_3`, ... suffix instead of overwriting the earlier log.
pub(crate) fn create_run_log(destination: &Path, stamp: &str, keep: usize) -> io::Result<(File, PathBuf)> {
    fs::create_dir_all(destination)?;

    let mut candidate = destination.join(format!("import-log-{stamp}.txt"));
    for suffix in 2..1000 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                prune_old_logs(destination, keep);
                return Ok((file, candidate));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                candidate = destination.join(format!("import-log-{stamp}_{suffix}.txt"));
            }
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many log files for this run",
    ))
}

/// Delete every run log in `destination`, returning how many were removed.
pub(crate) fn delete_run_logs(destination: &Path) -> io::Result<usize> {
    let mut removed = 0;

    for entry in fs::read_dir(destination)?.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if !is_run_log(&path) {
            continue;
        }
        // A log held open elsewhere stays put; the rest still go.
        if fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }

    Ok(removed)
}

/// The newest run log in `destination`, if it holds any. The fixed-width
/// timestamp in the name means a plain name comparison orders them by age.
pub(crate) fn newest_run_log(destination: &Path) -> Option<PathBuf> {
    fs::read_dir(destination)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.path())
        .filter(|path| is_run_log(path))
        .max_by(|left, right| left.file_name().cmp(&right.file_name()))
}

/// Open `path` in Notepad, whatever program owns the `.txt` association.
pub(crate) fn open_in_notepad(path: &Path) -> io::Result<()> {
    std::process::Command::new("notepad.exe")
        .arg(path)
        .spawn()?;

    Ok(())
}

/// The first thing the window says. A dry run remembered from last time is worth
/// saying out loud, so a run that only previews does not come as a surprise.
pub(crate) fn startup_status(dry_run: bool) -> &'static str {
    if dry_run {
        "Dry run is still ticked from last time: Start will only list where files would go. Untick it to import."
    } else {
        "Pick a source folder and a destination folder, then press Start import."
    }
}

/// Whether a file is one of the run logs this app writes.
pub(crate) fn is_run_log(path: &Path) -> bool {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => name.starts_with("import-log-") && name.ends_with(".txt"),
        None => false,
    }
}

/// Delete older run logs so a destination keeps only the newest few, returning
/// how many were removed.
pub(crate) fn prune_old_logs(destination: &Path, keep: usize) -> usize {
    let keep = keep.clamp(MIN_LOGS_TO_KEEP, MAX_LOGS_TO_KEEP);

    let Ok(entries) = fs::read_dir(destination) else {
        return 0;
    };

    let mut logs: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.path())
        .filter(|path| is_run_log(path))
        .collect();

    if logs.len() <= keep {
        return 0;
    }

    // The names carry a fixed-width timestamp, so sorting by name orders them by
    // age: everything before the last `keep` entries is the old end.
    logs.sort();
    let excess = logs.len() - keep;
    let mut removed = 0;
    for path in logs.into_iter().take(excess) {
        if fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }

    removed
}

/// A path in the form used for comparisons: real path when it exists, otherwise
/// the text cleaned up, with separators and case normalised.
pub(crate) fn path_key(path: &str) -> String {
    let real = fs::canonicalize(path)
        .map(|found| found.to_string_lossy().into_owned())
        .unwrap_or_else(|_| clean_folder(path));

    // A path that exists comes back canonical with a `\\?\` prefix, while one that
    // does not exist yet keeps its plain form; dropping the prefix lets the two be
    // compared, which is what the nesting checks need.
    let plain = real.strip_prefix("\\\\?\\").unwrap_or(real.as_str());
    let trimmed = plain.trim_end_matches(|c| c == '\\' || c == '/');
    trimmed.replace('/', "\\").to_ascii_lowercase()
}

/// The nearest folder that exists at or above `path`, so a destination that has
/// not been created yet can still be measured.
pub(crate) fn existing_ancestor(path: &Path) -> Option<&Path> {
    let mut current = Some(path);

    while let Some(candidate) = current {
        if candidate.is_dir() {
            return Some(candidate);
        }
        current = candidate.parent();
    }

    None
}

/// Free space on the drive holding `path`, in bytes. `None` when Windows cannot
/// answer, which leaves the run unwarned rather than blocked.
pub(crate) fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        unsafe extern "system" {
            fn GetDiskFreeSpaceExW(
                directory: *const u16,
                available: *mut u64,
                total: *mut u64,
                total_free: *mut u64,
            ) -> i32;
        }

        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);

        let (mut available, mut total, mut total_free) = (0u64, 0u64, 0u64);
        // SAFETY: the path is a NUL-terminated wide string and the three
        // out-pointers stay valid for the length of the call.
        let ok = unsafe {
            GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut total_free)
        };

        (ok != 0).then_some(available)
    }

    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

/// How many bytes the pictures and sidecars under `sources` add up to, skipping
/// the destination subtree the way the import itself does.
pub(crate) fn photos_bytes(sources: &[PathBuf], destination: &Path) -> u64 {
    let skip = plain(destination).to_string_lossy().to_ascii_lowercase();
    let mut total = 0;

    for source in sources {
        for entry in walkdir::WalkDir::new(source).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();
            let text = path.to_string_lossy().to_ascii_lowercase();
            if !skip.is_empty() && text.starts_with(&format!("{skip}\\")) {
                continue;
            }
            if !(crate::importer::is_importable(path) || crate::importer::is_sidecar(path))
            {
                continue;
            }

            if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }

    total
}

/// How many files an import would have taken from `folder`: the photos, videos
/// and sidecars the walk finds, sub-folders included.
pub(crate) fn folder_file_count(folder: &Path) -> usize {
    let mut count = 0;

    for entry in walkdir::WalkDir::new(folder)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if crate::importer::is_importable(path) || crate::importer::is_sidecar(path) {
            count += 1;
        }
    }

    count
}

/// The warning to show when the destination drive holds less free space than the
/// photos look like needing.
pub(crate) fn low_space_warning(sources: &[PathBuf], destination: &Path) -> Option<String> {
    let free = free_bytes(existing_ancestor(destination)?)?;
    let needed = photos_bytes(sources, destination);
    if needed == 0 {
        return None;
    }

    // Copies are the same size as the originals; the headroom covers sidecars
    // and the filesystem's own bookkeeping.
    let needed_with_headroom = needed + needed / 50;
    if free >= needed_with_headroom {
        return None;
    }

    let drive = destination
        .components()
        .next()
        .map(|prefix| prefix.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_else(|| "The destination drive".to_owned());

    Some(format!(
        "{drive} has {} free but about {} of photos and sidecars to copy; the run may run out of space.",
        size_label(free),
        size_label(needed)
    ))
}

/// How the destination and a source folder clash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Overlap {
    /// The very same folder.
    Same,
    /// The destination sits somewhere under the source.
    DestinationInsideSource,
    /// The source sits somewhere under the destination.
    SourceInsideDestination,
}

/// Every source folder the destination overlaps, and how. A matching prefix that
/// stops mid-name, `E:\card` against `E:\card2`, is a different folder.
pub(crate) fn overlap_pairs(sources: &[String], destination: &str) -> Vec<(Overlap, String)> {
    let destination_key = path_key(destination);
    if destination_key.is_empty() {
        return Vec::new();
    }

    let nested = |outer: &str, inner: &str| inner.starts_with(&format!("{outer}\\"));

    let mut clashes = Vec::new();
    for source in sources {
        let source_key = path_key(source);
        if source_key.is_empty() {
            continue;
        }

        let kind = if destination_key == source_key {
            Overlap::Same
        } else if nested(&source_key, &destination_key) {
            Overlap::DestinationInsideSource
        } else if nested(&destination_key, &source_key) {
            Overlap::SourceInsideDestination
        } else {
            continue;
        };

        clashes.push((kind, source.trim().to_owned()));
    }

    clashes
}

/// The refusal Start shows when the safety tick is on and the destination
/// overlaps a source at all, nested either way included.
pub(crate) fn overlap_refusal(sources: &[String], destination: &str) -> Option<String> {
    let clashes = overlap_clashes(sources, destination);
    if clashes.is_empty() {
        return None;
    }

    Some(format!(
        "Nothing started: {}, and refusing an overlap like that is ticked. Untick it or choose another destination.",
        clashes.join("; ")
    ))
}

/// The same refusal, worded for the greyed-out Start button, whose hint is read
/// before anything is pressed.
pub(crate) fn overlap_refusal_hint(sources: &[String], destination: &str) -> Option<String> {
    let clashes = overlap_clashes(sources, destination);
    if clashes.is_empty() {
        return None;
    }

    Some(format!(
        "Greyed out: {}, and refusing an overlap like that is ticked. Untick it or choose another destination.",
        clashes.join("; ")
    ))
}

/// The source folders that are not on disk, keeping the order they were listed.
pub(crate) fn missing_sources(sources: &[String]) -> Vec<String> {
    sources
        .iter()
        .filter(|folder| !Path::new(folder).is_dir())
        .map(|folder| folder.trim().to_owned())
        .collect()
}

/// The phrase the refusal and the greyed-out Start are both built from, naming
/// every source folder that is not there.
pub(crate) fn missing_source_clash(sources: &[String]) -> Option<String> {
    let missing = missing_sources(sources);
    if missing.is_empty() {
        return None;
    }

    let list = missing.join("; ");
    Some(if missing.len() == 1 {
        format!("there is no folder at {list}")
    } else {
        format!("there are no folders at {list}")
    })
}

/// How the folders clash, one phrase per pair, as the refusal and its hint list.
pub(crate) fn overlap_clashes(sources: &[String], destination: &str) -> Vec<String> {
    overlap_pairs(sources, destination)
        .into_iter()
        .map(|(kind, folder)| match kind {
            Overlap::Same => format!("the destination is the same folder as the source {folder}"),
            Overlap::DestinationInsideSource => {
                format!("the destination is inside the source {folder}")
            }
            Overlap::SourceInsideDestination => {
                format!("the source {folder} is inside the destination")
            }
        })
        .collect()
}

/// The warnings the destination's clashes with a source folder deserve - the
/// same folder, inside one, or a source inside it - one per pair, so a run that
/// clashes twice says so twice.
pub(crate) fn destination_overlaps(sources: &[String], destination: &str) -> Vec<String> {
    overlap_pairs(sources, destination)
        .into_iter()
        .map(|(kind, folder)| match kind {
            Overlap::Same => format!(
                "The destination is the same folder as the source {folder}; files will be copied into the tree they are read from."
            ),
            Overlap::DestinationInsideSource => format!(
                "The destination is inside the source {folder}; files will be copied into the tree they are read from."
            ),
            Overlap::SourceInsideDestination => format!(
                "The source {folder} is inside the destination; it is skipped, so the library is never imported into itself."
            ),
        })
        .collect()
}

/// Tidy a path that was typed, pasted or copied as text: the quotes Explorer's
/// "Copy as path" adds go, along with stray spaces and line breaks. A path that
/// names a single file - a photo copied out of Explorer, or one with a photo or
/// video extension that is not on disk yet - becomes the folder holding it, since
/// that is what the fields and the queue import from.
pub(crate) fn clean_folder(text: &str) -> String {
    let trimmed = text.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed);
    let cleaned = unquoted.trim();

    let path = Path::new(cleaned);
    let names_a_file = path.is_file() || (!path.exists() && crate::importer::is_importable(path));
    if !names_a_file {
        return cleaned.to_owned();
    }

    match path.parent() {
        // A drive root is a folder of its own; a bare file name has no folder to
        // fall back on, so both keep whatever was written.
        Some(parent) if !parent.as_os_str().is_empty() => {
            parent.to_string_lossy().into_owned()
        }
        _ => cleaned.to_owned(),
    }
}

/// A field's Recent list with another list folded in behind it: the field's own
/// folders first, then the folders only the other list knows, with pinned
/// favourites at the front. Folders often get used both as a source and as a
/// destination, so each list can offer the other's history.
pub(crate) fn merged_recents(primary: &[String], other: &[String], pinned: &[String]) -> Vec<String> {
    let mut list = primary.to_vec();

    for folder in other {
        let folder = folder.trim();
        if folder.is_empty() {
            continue;
        }
        if list.iter().any(|known| known.eq_ignore_ascii_case(folder)) {
            continue;
        }
        list.push(folder.to_owned());
    }

    Settings::pinned_first(&mut list, pinned);
    list
}

/// Drop a folder from one recent list, and stop treating it as a favourite.
pub(crate) fn forget_recent(list: &mut Vec<String>, pinned: &mut Vec<String>, folder: &str) {
    let folder = folder.trim();
    list.retain(|entry| !entry.eq_ignore_ascii_case(folder));
    Settings::set_pinned(pinned, folder, false);
}

/// The import position shown beside a queued folder: the source field is 1, so
/// the first queued folder is 2.
pub(crate) fn queue_position(index: usize) -> usize {
    index + 2
}

/// Move the item at `from` to `to`, keeping the rest in order.
#[cfg(test)]
pub(crate) fn move_item(items: &mut Vec<String>, from: usize, to: usize) -> bool {
    if from >= items.len() || to >= items.len() || from == to {
        return false;
    }

    let item = items.remove(from);
    items.insert(to, item);
    true
}

/// One line per source folder, pairing each with what became of its files: how
/// many were imported, skipped as duplicates, failed, and the total, so every file
/// in that folder is accounted for.
pub(crate) fn source_summary(
    folders: &[String],
    imported: &[usize],
    duplicates: &[usize],
    failed: &[usize],
) -> String {
    let rows = imported.len().max(duplicates.len()).max(failed.len());
    let width = (0..rows)
        .filter_map(|index| folders.get(index))
        .map(|folder| folder.chars().count())
        .max()
        .unwrap_or(0)
        .max("folder".len());

    let mut text = format!(
        "  {:<width$}  {:>8}  {:>6}  {:>6}  {:>6}",
        "folder",
        "imported",
        "dups",
        "failed",
        "total",
        width = width
    );

    for index in 0..rows {
        let Some(folder) = folders.get(index) else {
            continue;
        };

        let imported = imported.get(index).copied().unwrap_or(0);
        let duplicates = duplicates.get(index).copied().unwrap_or(0);
        let failed = failed.get(index).copied().unwrap_or(0);
        let total = imported + duplicates + failed;

        text.push('\n');
        text.push_str(&format!(
            "  {folder:<width$}  {imported:>8}  {duplicates:>6}  {failed:>6}  {total:>6}",
            width = width
        ));
    }

    text
}

/// Human-readable file size, such as `932 B`, `812.4 KB` or `4.2 MB`.
pub(crate) fn size_label(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let amount = bytes as f64;
    if amount < KB {
        format!("{bytes} B")
    } else if amount < MB {
        format!("{:.1} KB", amount / KB)
    } else if amount < GB {
        format!("{:.1} MB", amount / MB)
    } else {
        format!("{:.1} GB", amount / GB)
    }
}

/// The folder a dropped item stands for: itself when it is a folder, and the
/// folder holding it when it is a file.
pub(crate) fn dropped_folder(path: &Path) -> Option<PathBuf> {
    if path.is_dir() {
        return Some(path.to_path_buf());
    }

    path.parent()
        .filter(|parent| parent.is_dir())
        .map(|parent| parent.to_path_buf())
}


/// The folders a set of dropped items stand for, in the order they were dropped,
/// with repeats collapsed: many photos from one folder count once, photos from
/// different folders each count.
pub(crate) fn dropped_folders(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = Vec::new();

    for path in paths {
        let Some(folder) = dropped_folder(path) else {
            continue;
        };

        let key = path_key(&folder.to_string_lossy());
        let seen = folders
            .iter()
            .any(|known| path_key(&known.to_string_lossy()) == key);
        if !seen {
            folders.push(folder);
        }
    }

    folders
}

/// Count one photo against the dated folder it landed in.
pub(crate) fn record_day(counts: &mut BTreeMap<String, usize>, target: &Path) {
    if let Some(day) = day_folder(target) {
        *counts.entry(day).or_insert(0) += 1;
    }
}

/// The `year-year-month-day` folder a destination file landed in.
pub(crate) fn day_folder(target: &Path) -> Option<String> {
    target
        .parent()?
        .file_name()?
        .to_str()
        .map(|name| name.to_owned())
}

/// What a keyboard shortcut asks the window to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(test)]
pub(crate) enum Shortcut {
    Start,
    Cancel,
    CopySummary,
    CopyLog,
    OpenLog,
    ShowLog,
    NewestLog,
}

/// The key presses worth answering this frame, gathered so the mapping can be
/// tested without a window.
#[derive(Clone, Copy, Debug, Default)]
#[cfg(test)]
pub(crate) struct Keys {
    ctrl_enter: bool,
    /// A bare Enter, with no Ctrl, which only starts when no text field wants it.
    enter: bool,
    escape: bool,
    copy_summary: bool,
    copy_log: bool,
    open_log: bool,
    show_log: bool,
    newest_log: bool,
    /// A text field has the keyboard, where the keys belong to the text.
    typing: bool,
    /// A run is going, which Escape may stop even from a path field.
    running: bool,
}

/// The shortcut a key press stands for, if any. The log shortcuts are ignored
/// while a text field has the keyboard, where they only mean typing; Ctrl+Enter
/// is always honoured, since it cannot be part of a path, and Escape stops a run
/// that is going even mid-typing, because stopping is the one thing worth having
/// there. Escape with nothing running still just leaves the field. A bare Enter
/// starts a run too, but only when no field has the keyboard, where Enter is the
/// key that confirms the path being typed.
#[cfg(test)]
pub(crate) fn shortcut_action(keys: Keys) -> Option<Shortcut> {
    if keys.ctrl_enter {
        return Some(Shortcut::Start);
    }
    if keys.escape && keys.running {
        return Some(Shortcut::Cancel);
    }
    if keys.typing {
        return None;
    }
    if keys.enter {
        return Some(Shortcut::Start);
    }
    if keys.copy_summary {
        return Some(Shortcut::CopySummary);
    }
    if keys.copy_log {
        return Some(Shortcut::CopyLog);
    }
    if keys.open_log {
        return Some(Shortcut::OpenLog);
    }
    if keys.show_log {
        return Some(Shortcut::ShowLog);
    }
    if keys.newest_log {
        return Some(Shortcut::NewestLog);
    }
    if keys.escape {
        return Some(Shortcut::Cancel);
    }

    None
}

/// A rough "about 2 min left" for a run, from how fast the finished files went.
pub(crate) fn estimate_left(done: usize, total: usize, elapsed: Duration) -> Option<String> {
    if done == 0 || total == 0 || done >= total {
        return None;
    }

    let per_file = elapsed.as_secs_f64() / done as f64;
    let remaining = per_file * (total - done) as f64;

    Some(format!("about {} left", duration_label(remaining)))
}

/// A duration in the words the estimate uses: `45 s`, `2 min`, `1 h 12 min`.
pub(crate) fn duration_label(seconds: f64) -> String {
    let seconds = seconds.max(0.0).ceil();

    if seconds < 60.0 {
        return format!("{seconds} s");
    }

    let minutes = (seconds / 60.0).ceil() as u64;
    if minutes < 60 {
        return format!("{minutes} min");
    }

    let hours = minutes / 60;
    let rest = minutes % 60;
    if rest == 0 {
        format!("{hours} h")
    } else {
        format!("{hours} h {rest} min")
    }
}

/// The span of dates the run covered, oldest folder to newest.
pub(crate) fn date_span(counts: &BTreeMap<String, usize>) -> Option<String> {
    let oldest = counts.keys().next()?;
    let newest = counts.keys().next_back()?;

    if oldest == newest {
        return Some(format!("Dates:  {oldest}"));
    }

    let days = counts.len();
    Some(format!(
        "Dates:  {oldest} to {newest} (over {days} days)"
    ))
}

/// The day counts added up by year, as one line: `2025: 1   2026: 36`.
pub(crate) fn year_summary(counts: &BTreeMap<String, usize>) -> String {
    let mut years: BTreeMap<&str, usize> = BTreeMap::new();

    for (day, count) in counts {
        let Some(year) = day.get(0..4) else {
            continue;
        };
        *years.entry(year).or_insert(0) += count;
    }

    let parts: Vec<String> = years
        .iter()
        .map(|(year, count)| format!("{year}: {count}"))
        .collect();

    parts.join("   ")
}

/// One aligned line per dated folder, oldest first.
pub(crate) fn day_summary(counts: &BTreeMap<String, usize>) -> String {
    let mut text = String::new();

    for (day, count) in counts {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("  {day}  {count:>5}"));
    }

    text
}

/// Whether two placements differ by more than half a point.
pub(crate) fn differs(left: (f32, f32), right: (f32, f32)) -> bool {
    (left.0 - right.0).abs() > 0.5 || (left.1 - right.1).abs() > 0.5
}

/// Open a log in the chosen program, or in Notepad when none is set.
pub(crate) fn open_log_in(editor: Option<&Path>, path: &Path) -> io::Result<()> {
    match editor {
        Some(program) => Command::new(program).arg(path).spawn().map(|_| ()),
        None => open_in_notepad(path),
    }
}

/// Open a text file in whatever program is registered for `.txt` files.
pub(crate) fn open_in_editor(path: &Path) -> io::Result<()> {
    // Explorer hands the file to the shell association, so the user's own editor
    // opens instead of a fixed one - Notepad only if it is still their choice.
    Command::new("explorer.exe").arg(path).spawn().map(|_| ())
}

/// The `/select,<path>` token Explorer needs, quoted so spaces stay inside the
/// path instead of splitting the argument.
pub(crate) fn select_arg(path: &Path) -> String {
    format!("/select,\"{}\"", path.display())
}

/// Show a file in Explorer with the file itself selected.
pub(crate) fn reveal_in_explorer(path: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;

    let mut command = Command::new("explorer.exe");
    // Rust's normal argument quoting escapes with backslashes, which Explorer
    // does not understand, so the token goes in raw.
    command.raw_arg(select_arg(path));
    command.spawn().map(|_| ())
}

/// Show a folder in Windows File Explorer.
pub(crate) fn open_folder(path: &Path) -> io::Result<()> {
    // Explorer takes the folder as its argument and returns immediately; a
    // non-zero exit status from it is normal and not worth reporting.
    Command::new("explorer.exe").arg(path).spawn().map(|_| ())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropTarget {
    Source,
    Destination,
}


pub(crate) fn relative(root: &Path, path: &Path) -> String {
    if let Ok(stripped) = path.strip_prefix(root) {
        return stripped.display().to_string();
    }

    // The engine works on canonical paths (`\\?\D:\...` on Windows) while the
    // UI holds the paths as typed, so compare without the verbatim prefix.
    let plain_root = plain(root);
    let plain_path = plain(path);
    plain_path
        .strip_prefix(&plain_root)
        .unwrap_or(&plain_path)
        .display()
        .to_string()
}

pub(crate) fn plain(path: &Path) -> PathBuf {
    match path.to_string_lossy().strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => path.to_path_buf(),
    }
}

impl ImporterApp {
    /// Take every event the worker has posted so far. Returns whether a run is
    /// still going, so the window knows when to stop looking.
    pub(crate) fn drain_events(&mut self) -> bool {
        let mut batch = Vec::new();
        if let Some(receiver) = &self.events {
            while let Ok(event) = receiver.try_recv() {
                batch.push(event);
            }
        }
        for event in batch {
            self.handle_event(event);
        }
        self.running
    }

    /// This run's whole log, every file line included, ready for the clipboard.
    pub(crate) fn log_text(&mut self) -> Option<String> {
        if self.running || !self.log_file_present() {
            return None;
        }

        let Some(path) = self.log_path.clone() else {
            return None;
        };

        match std::fs::read_to_string(&path) {
            Ok(text) => {
                self.status = "Run log copied to the clipboard.".to_owned();
                Some(text)
            }
            Err(error) => {
                self.error = Some(format!("Could not read {}: {error}", path.display()));
                None
            }
        }
    }

    /// The last run's summary, ready for the clipboard.
    pub(crate) fn summary_text(&mut self) -> Option<String> {
        let text = self.last_summary.clone();
        if text.is_some() {
            self.status = "Run summary copied to the clipboard.".to_owned();
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ImporterApp, clean_folder, create_run_log, day_folder, day_summary,
        destination_overlaps, dropped_folder, dropped_folders,
        duration_label, estimate_left, folder_file_count, existing_ancestor, forget_recent,
        merged_recents, free_bytes, is_run_log, low_space_warning, move_item,
        date_span, missing_folder_note, newest_run_log, photos_bytes, queue_position, record_day,
        overlap_refusal, select_arg,
        shortcut_action, size_label, Keys,
        year_summary, Shortcut,
    };
    use super::startup_status;
    use crate::importer::TransferMode;
        use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    #[test]
    fn the_day_folder_is_the_parent_of_the_target() {
        assert_eq!(
            day_folder(Path::new(r"E:\Library\2026\2026-06-01\IMG_0001.JPG")),
            Some("2026-06-01".to_owned())
        );
        assert_eq!(day_folder(Path::new("IMG_0001.JPG")), None);
    }

    #[test]
    fn record_day_counts_photos_per_folder() {
        let mut counts = BTreeMap::new();

        record_day(&mut counts, Path::new(r"E:\L\2026\2026-06-01\a.jpg"));
        record_day(&mut counts, Path::new(r"E:\L\2026\2026-06-01\b.jpg"));
        record_day(&mut counts, Path::new(r"E:\L\2026\2026-06-02\c.jpg"));

        assert_eq!(counts.len(), 2);
        assert_eq!(counts.get("2026-06-01"), Some(&2));
        assert_eq!(counts.get("2026-06-02"), Some(&1));
    }

    #[test]
    fn day_summary_lists_each_folder_in_date_order() {
        let mut counts = BTreeMap::new();
        counts.insert("2026-06-02".to_owned(), 3);
        counts.insert("2025-12-31".to_owned(), 1);
        counts.insert("2026-06-01".to_owned(), 12);

        assert_eq!(
            day_summary(&counts),
            "  2025-12-31      1\n  2026-06-01     12\n  2026-06-02      3"
        );
    }

    #[test]
    fn day_summary_is_empty_without_imports() {
        assert!(day_summary(&BTreeMap::new()).is_empty());
    }

    fn summary_of(imported: usize) -> crate::importer::Summary {
        crate::importer::Summary {
            imported,
            duplicates: 0,
            failed: 0,
            sidecars: 0,
            bytes: 0,
            per_source: vec![imported],
            per_source_duplicates: vec![0],
            per_source_failed: vec![0],
            stray_sidecars: Vec::new(),
            total: imported,
            elapsed: std::time::Duration::from_secs(1),
        }
    }

    #[test]
    fn finishing_the_run_logs_the_day_table_in_the_pane_and_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let (file, path) = create_run_log(directory.path(), "2026-11-06_204512", 20).unwrap();

        let mut app = ImporterApp::default();
        app.log_file = Some(file);
        app.day_counts.insert("2026-06-01".to_owned(), 2);
        app.day_counts.insert("2026-06-02".to_owned(), 1);

        app.finish(&summary_of(3), "Done");

        let written = std::fs::read_to_string(&path).unwrap();
        for text in [&written, &app.log] {
            assert!(text.contains("Done: 3 imported"), "missing totals: {text}");
            assert!(
                text.contains("Files per day folder:\n  2026-06-01      2\n  2026-06-02      1"),
                "missing day table: {text}"
            );
        }
    }

    #[test]
    fn a_dry_run_marks_the_day_table_as_planned() {
        let mut app = ImporterApp::default();
        app.dry_run = true;
        app.day_counts.insert("2026-06-01".to_owned(), 4);

        app.finish(&summary_of(4), "Dry run");

        assert!(app.log.contains("Files per day folder (planned):"));
    }

    #[test]
    fn a_dropped_file_stands_for_the_folder_that_holds_it() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("shoot");
        std::fs::create_dir_all(&folder).unwrap();
        let photo = folder.join("IMG_0001.JPG");
        std::fs::write(&photo, b"jpeg").unwrap();

        assert_eq!(dropped_folder(&photo), Some(folder.clone()));
        assert_eq!(dropped_folder(&folder), Some(folder));
    }

    #[test]
    fn a_dropped_file_whose_folder_is_gone_is_rejected() {
        assert_eq!(dropped_folder(Path::new(r"D:\missing\ghost.jpg")), None);
    }

    #[test]
    fn sizes_are_shown_in_readable_units() {
        assert_eq!(size_label(0), "0 B");
        assert_eq!(size_label(1023), "1023 B");
        assert_eq!(size_label(1024), "1.0 KB");
        assert_eq!(size_label(1536), "1.5 KB");
        assert_eq!(size_label(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(size_label(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn queued_folders_are_numbered_after_the_source_field() {
        assert_eq!(queue_position(0), 2, "the first queued folder follows the field");
        assert_eq!(queue_position(2), 4);
    }

    #[test]
    fn queued_sources_are_deduped_and_kept_in_order() {
        let mut app = ImporterApp::default();
        app.source = r"D:\one".to_owned();

        assert!(!app.add_extra_source(r"d:\ONE"), "already the first folder");
        assert!(app.add_extra_source(r"D:\two"));
        assert!(!app.add_extra_source(r"D:\TWO"), "already queued");
        assert!(!app.add_extra_source("   "), "blank");

        assert_eq!(
            app.source_folders(),
            vec!["D:\\one".to_owned(), "D:\\two".to_owned()]
        );
    }

    #[test]
    fn reset_clears_the_fields_and_returns_to_copy() {
        let mut app = ImporterApp::default();
        app.source = r"D:\in".to_owned();
        app.destination = r"E:\out".to_owned();
        app.recent_sources = vec![r"D:\in".to_owned()];
        app.recent_destinations = vec![r"E:\out".to_owned()];
        app.mode = TransferMode::Move;

        app.clear_memory();

        assert!(app.source.is_empty());
        assert!(app.destination.is_empty());
        assert!(app.recent_sources.is_empty());
        assert!(app.recent_destinations.is_empty());
        assert_eq!(app.mode, TransferMode::Copy);
        assert!(app.status.contains("cleared"));
    }

    #[test]
    fn each_run_gets_its_own_log_file() {
        let directory = tempfile::tempdir().unwrap();

        let (first, first_path) =
            create_run_log(directory.path(), "2026-11-06_204512", 20).unwrap();
        let (_second, second_path) =
            create_run_log(directory.path(), "2026-11-06_204512", 20).unwrap();

        assert_ne!(first_path, second_path);
        assert!(first_path.is_file());
        assert!(second_path.is_file());
        assert!(
            first_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("import-log-2026-11-06_204512"),
            "unexpected name: {}",
            first_path.display()
        );
        drop(first);
    }

    #[test]
    fn the_run_summary_can_be_copied_as_text() {
        let mut app = ImporterApp::default();
        app.day_counts.insert("2026-06-01".to_owned(), 2);
        app.day_counts.insert("2026-06-02".to_owned(), 1);

        assert!(app.last_summary.is_none(), "nothing before a run");

        app.finish(&summary_of(3), "Done");

        let copied = app.last_summary.clone().expect("a summary after the run");
        assert!(copied.starts_with("Done: 3 imported"), "{copied}");
        assert!(
            copied.contains("Files per day folder:\n  2026-06-01      2\n  2026-06-02      1"),
            "{copied}"
        );
        assert!(copied.contains("Files per year:  2026: 3"), "{copied}");
    }

    #[test]
    fn free_space_is_reported_for_a_real_folder() {
        let free = free_bytes(&std::env::temp_dir()).expect("free space");
        assert!(free > 0, "{free}");
    }

    #[test]
    fn a_destination_that_does_not_exist_yet_is_measured_from_its_parent() {
        let temp = std::env::temp_dir();
        let missing = temp.join("image-importer-not-here").join("nor-here");

        assert_eq!(
            existing_ancestor(&missing).map(Path::to_path_buf),
            Some(temp)
        );
    }

    #[test]
    fn photos_bytes_leaves_out_what_will_not_be_copied() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("card");
        std::fs::create_dir_all(source.join("sub")).expect("sub folder");
        std::fs::write(source.join("a.jpg"), vec![0u8; 100]).expect("photo");
        std::fs::write(source.join("sub").join("b.nef"), vec![0u8; 50]).expect("raw");
        std::fs::write(source.join("notes.txt"), vec![0u8; 999]).expect("notes");

        let destination = source.join("library");
        std::fs::create_dir_all(&destination).expect("destination");
        std::fs::write(destination.join("old.jpg"), vec![0u8; 777]).expect("already imported");

        assert_eq!(photos_bytes(&[source], &destination), 150);
    }

    #[test]
    fn nothing_is_said_about_space_when_the_drive_has_room() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("a.jpg"), vec![0u8; 10]).expect("photo");

        let sources = vec![dir.path().to_path_buf()];
        assert!(low_space_warning(&sources, dir.path()).is_none());
    }

    #[test]
    fn the_counters_row_tallies_sidecars_too() {
        let mut app = ImporterApp::default();
        app.imported = 5;
        app.duplicates = 1;
        app.failed = 2;
        app.sidecars = 3;

        assert_eq!(
            app.counters_label(),
            "Imported 5   ·   duplicates 1   ·   failed 2   ·   sidecars 3"
        );

        app.dry_run = true;
        assert!(
            app.counters_label().starts_with("Planned 5"),
            "a dry run counts planned files"
        );

        // A finished run takes the engine's own tally, which is what the row and
        // the summary line have to agree on.
        app.dry_run = false;
        app.finish(&summary_of(3), "Done");
        assert_eq!(app.imported, 3);
        assert_eq!(app.sidecars, 0);
    }

    #[test]
    fn shortcuts_map_keys_to_start_and_cancel() {
        assert_eq!(
            shortcut_action(Keys {
                ctrl_enter: true,
                ..Keys::default()
            }),
            Some(Shortcut::Start)
        );
        // Ctrl+Enter still starts while a path field has the keyboard.
        assert_eq!(
            shortcut_action(Keys {
                ctrl_enter: true,
                typing: true,
                ..Keys::default()
            }),
            Some(Shortcut::Start)
        );

        assert_eq!(
            shortcut_action(Keys {
                escape: true,
                ..Keys::default()
            }),
            Some(Shortcut::Cancel)
        );
        // Enter starts a run when the window has the keyboard.
        assert_eq!(
            shortcut_action(Keys {
                enter: true,
                ..Keys::default()
            }),
            Some(Shortcut::Start)
        );
        // Enter while a path field is focused confirms the path instead.
        assert_eq!(
            shortcut_action(Keys {
                enter: true,
                typing: true,
                ..Keys::default()
            }),
            None
        );

        // Escape stops a run even with a path field focused.
        assert_eq!(
            shortcut_action(Keys {
                escape: true,
                typing: true,
                running: true,
                ..Keys::default()
            }),
            Some(Shortcut::Cancel)
        );

        // Escape while typing only leaves the field.
        assert_eq!(
            shortcut_action(Keys {
                escape: true,
                typing: true,
                ..Keys::default()
            }),
            None
        );

        assert_eq!(shortcut_action(Keys::default()), None);
    }

    #[test]
    fn shortcuts_reach_the_log_buttons() {
        for (keys, expected) in [
            (
                Keys {
                    copy_summary: true,
                    ..Keys::default()
                },
                Shortcut::CopySummary,
            ),
            (
                Keys {
                    copy_log: true,
                    ..Keys::default()
                },
                Shortcut::CopyLog,
            ),
            (
                Keys {
                    open_log: true,
                    ..Keys::default()
                },
                Shortcut::OpenLog,
            ),
            (
                Keys {
                    show_log: true,
                    ..Keys::default()
                },
                Shortcut::ShowLog,
            ),
            (
                Keys {
                    newest_log: true,
                    ..Keys::default()
                },
                Shortcut::NewestLog,
            ),
        ] {
            assert_eq!(shortcut_action(keys), Some(expected));
        }

        // A path field keeps its keys for its own text.
        assert_eq!(
            shortcut_action(Keys {
                copy_summary: true,
                typing: true,
                ..Keys::default()
            }),
            None
        );
    }

    #[test]
    fn the_whole_log_can_be_copied_once_it_exists() {
        let mut app = ImporterApp::default();
        let status = app.status.clone();
        let _ = app.log_text();
        assert!(app.error.is_none());
        assert_eq!(app.status, status, "nothing to copy yet");

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("import-log-1.txt");
        std::fs::write(&path, "first line\nsecond line\n").unwrap();
        app.log_path = Some(path.clone());

        let _ = app.log_text();
        assert_eq!(app.status, "Run log copied to the clipboard.");
        assert!(app.error.is_none(), "{:?}", app.error);

        // A log that has gone takes the button with it, so copying does nothing.
        std::fs::remove_file(&path).unwrap();
        let status = app.status.clone();
        let _ = app.log_text();
        assert_eq!(app.status, status);
        assert!(app.error.is_none(), "{:?}", app.error);
    }

    #[test]
    fn the_log_shortcuts_do_nothing_without_a_log_or_a_summary() {
        let mut app = ImporterApp::default();
        let status = app.status.clone();

        app.open_log();
        app.show_log();
        let _ = app.summary_text();
        app.newest_log();

        assert!(app.error.is_none(), "nothing to complain about: {:?}", app.error);
        assert_eq!(app.status, status, "and nothing to report either");
    }

    #[test]
    fn shortcuts_only_act_when_they_can() {
        let mut app = ImporterApp::default();
        assert!(!app.can_start(), "no folders yet");

        app.source = r"D:\in".to_owned();
        app.destination = r"E:\out".to_owned();
        assert!(app.can_start());

        // One run at a time: Ctrl+Enter during an import does nothing.
        app.running = true;
        assert!(!app.can_start());
    }

    #[test]
    fn the_time_left_is_worked_out_from_the_files_so_far() {
        use std::time::Duration;

        // Four of twelve files in 20 s: the remaining eight should take 40 s.
        assert_eq!(
            estimate_left(4, 12, Duration::from_secs(20)).as_deref(),
            Some("about 40 s left")
        );

        // Two hundred of a thousand files in ten minutes leaves forty.
        assert_eq!(
            estimate_left(200, 1000, Duration::from_secs(600)).as_deref(),
            Some("about 40 min left")
        );

        // Nothing to go on before the first file, and nothing left at the end.
        assert!(estimate_left(0, 12, Duration::from_secs(5)).is_none());
        assert!(estimate_left(12, 12, Duration::from_secs(60)).is_none());
        assert!(estimate_left(1, 0, Duration::from_secs(1)).is_none());
    }

    #[test]
    fn durations_read_as_seconds_minutes_and_hours() {
        assert_eq!(duration_label(0.4), "1 s");
        assert_eq!(duration_label(45.0), "45 s");
        assert_eq!(duration_label(90.0), "2 min");
        assert_eq!(duration_label(3600.0), "1 h");
        assert_eq!(duration_label(4300.0), "1 h 12 min");
    }

    #[test]
    fn the_live_year_roll_up_follows_the_day_counts() {
        let mut app = ImporterApp::default();
        app.day_counts.insert("2025-12-31".to_owned(), 1);
        app.day_counts.insert("2026-06-01".to_owned(), 12);

        assert!(app.live_year_summary().is_none(), "nothing while idle");

        app.running = true;
        assert_eq!(
            app.live_year_summary().as_deref(),
            Some("Files per year so far:  2025: 1   2026: 12")
        );

        app.dry_run = true;
        assert_eq!(
            app.live_year_summary().as_deref(),
            Some("Files per year so far (planned):  2025: 1   2026: 12")
        );
    }

    #[test]
    fn the_summary_states_the_span_of_dates_it_covered() {
        let mut counts = BTreeMap::new();
        assert_eq!(date_span(&counts), None, "nothing imported, no dates");

        counts.insert("2026-06-01".to_owned(), 1);
        assert_eq!(date_span(&counts).as_deref(), Some("Dates:  2026-06-01"));

        counts.insert("2019-04-02".to_owned(), 4);
        counts.insert("2026-06-02".to_owned(), 3);
        assert_eq!(
            date_span(&counts).as_deref(),
            Some("Dates:  2019-04-02 to 2026-06-02 (over 3 days)")
        );
    }

    #[test]
    fn year_summary_rolls_the_days_up() {
        let mut counts = BTreeMap::new();
        counts.insert("2025-12-31".to_owned(), 1);
        counts.insert("2026-06-01".to_owned(), 12);
        counts.insert("2026-06-02".to_owned(), 3);

        assert_eq!(year_summary(&counts), "2025: 1   2026: 15");
        assert_eq!(year_summary(&BTreeMap::new()), "");
    }

    #[test]
    fn the_live_day_table_builds_up_while_running() {
        let mut app = ImporterApp::default();
        app.day_counts.insert("2026-06-01".to_owned(), 2);

        assert!(app.live_day_summary().is_none(), "nothing while idle");

        app.running = true;
        let table = app.live_day_summary().expect("a run in progress");
        assert!(
            table.starts_with("Files per day so far:\n  2026-06-01"),
            "{table}"
        );

        app.dry_run = true;
        let planned = app.live_day_summary().expect("a preview in progress");
        assert!(planned.contains("(planned)"), "{planned}");
    }

    #[test]
    fn the_summary_breaks_down_files_per_source_folder() {
        let mut app = ImporterApp::default();
        app.source = r"D:\card1".to_owned();
        app.extra_sources = vec![r"E:\card2".to_owned()];

        let mut summary = summary_of(3);
        summary.per_source = vec![2, 1];

        app.finish(&summary, "Done");

        summary.per_source_duplicates = vec![1, 0];
        summary.per_source_failed = vec![0, 1];

        app.finish(&summary, "Done");

        let expected = format!(
            "Files per source folder:\n  {:<8}  {:>8}  {:>6}  {:>6}  {:>6}\n  {:<8}  {:>8}  {:>6}  {:>6}  {:>6}\n  {:<8}  {:>8}  {:>6}  {:>6}  {:>6}",
            "folder", "imported", "dups", "failed", "total",
            r"D:\card1", 2, 1, 0, 3,
            r"E:\card2", 1, 0, 1, 2
        );
        assert!(app.log.contains(&expected), "{}", app.log);
    }

    #[test]
    fn the_per_source_table_accounts_for_every_file() {
        use super::source_summary;

        let folders = vec![r"D:\card1".to_owned(), r"E:\card2".to_owned()];
        let table = source_summary(&folders, &[2, 1], &[1, 0], &[0, 1]);

        assert!(table.starts_with("  folder "), "{table}");
        assert!(
            table.contains(r"D:\card1         2       1       0       3"),
            "{table}"
        );
        assert!(
            table.contains(r"E:\card2         1       0       1       2"),
            "{table}"
        );
    }

    #[test]
    fn a_single_source_needs_no_breakdown() {
        let mut app = ImporterApp::default();
        app.source = r"D:\card1".to_owned();

        app.finish(&summary_of(3), "Done");

        assert!(!app.log.contains("Files per source folder:"), "{}", app.log);
    }

    #[test]
    fn a_remembered_dry_run_is_announced_at_startup() {
        assert!(startup_status(true).contains("Dry run is still ticked"));
        assert!(startup_status(false).starts_with("Pick a source folder"));
    }

    #[test]
    fn the_tick_greys_out_start_for_a_nested_overlap() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("in");
        std::fs::create_dir_all(&source).unwrap();

        let mut app = ImporterApp::default();
        app.source = source.to_string_lossy().into_owned();
        app.destination = source.join("library").to_string_lossy().into_owned();

        assert!(app.can_start(), "both paths are filled in");
        assert!(
            app.start_blocked().is_none(),
            "without the tick Start stays available"
        );

        app.refuse_overlap = true;

        let hint = app.start_blocked().expect("the tick greys out this run");
        assert!(hint.starts_with("Greyed out:"), "{hint}");
        assert!(hint.contains("inside the source"), "{hint}");
    }

    #[test]
    fn a_missing_source_folder_is_refused_before_anything_runs() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("not-there");
        let destination = directory.path().join("library");
        std::fs::create_dir_all(&destination).unwrap();

        let mut app = ImporterApp::default();
        app.source = missing.to_string_lossy().into_owned();
        app.destination = destination.to_string_lossy().into_owned();

        assert!(app.can_start(), "both paths are filled in");
        let hint = app.start_blocked().expect("the missing folder greys Start");
        assert!(
            hint.starts_with("Greyed out: there is no folder at"),
            "{hint}"
        );
        assert!(hint.contains("not-there"), "{hint}");

        // Ctrl+Enter skips the button, so the same refusal has to reach the error
        // line, and nothing may start.
        app.start();
        assert!(!app.running, "nothing ran");
        let error = app.error.expect("an error explains why");
        assert!(
            error.starts_with("Nothing started: there is no folder at"),
            "{error}"
        );
    }

    #[test]
    fn a_second_missing_source_is_named_too() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("gone-one");
        let second = directory.path().join("gone-two");

        let clash = super::missing_source_clash(&[
            first.to_string_lossy().into_owned(),
            second.to_string_lossy().into_owned(),
        ])
        .expect("both are missing");

        assert!(clash.starts_with("there are no folders at"), "{clash}");
        assert!(clash.contains("gone-one"), "{clash}");
        assert!(clash.contains("gone-two"), "{clash}");
    }

    #[test]
    fn a_nested_overlap_warns_but_the_run_still_starts() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("in");
        std::fs::create_dir_all(&source).unwrap();

        let mut app = ImporterApp::default();
        app.source = source.to_string_lossy().into_owned();
        app.destination = source.join("library").to_string_lossy().into_owned();

        assert!(!app.refuse_overlap, "the tick is off unless asked for");
        let warnings = app.current_warnings();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("inside the source")),
            "{warnings:?}"
        );

        app.start();

        assert!(app.running, "a warned run still goes ahead");
        app.cancel_import();
    }

    #[test]
    fn a_destination_inside_a_source_is_warned_about() {
        let sources = vec![r"D:\card".to_owned(), r"E:\card2".to_owned()];

        assert!(destination_overlaps(&sources, r"F:\library").is_empty());

        let same = destination_overlaps(&sources, r"d:\CARD\");
        assert_eq!(same.len(), 1, "{same:?}");
        assert!(same[0].contains("same folder"), "{same:?}");

        let inside = destination_overlaps(&sources, r"E:\card2\DCIM\in");
        assert_eq!(inside.len(), 1, "{inside:?}");
        assert!(inside[0].contains("inside the source"), "{inside:?}");

        // The other way round: a source folder sitting inside the destination
        // would have the run reading the tree it copies into.
        let nested = vec![r"D:\photos\2024".to_owned()];
        let reversed = destination_overlaps(&nested, r"D:\photos");
        assert_eq!(reversed.len(), 1, "{reversed:?}");
        assert!(reversed[0].contains("inside the destination"), "{reversed:?}");

        // A matching prefix that stops mid-name is a different folder, either way.
        assert!(destination_overlaps(&sources, r"E:\card22").is_empty());
        assert!(destination_overlaps(&[r"D:\photos2".to_owned()], r"D:\photos").is_empty());
    }

    #[test]
    fn every_clashing_folder_is_listed_at_once() {
        let sources = vec![
            r"D:\photos".to_owned(),
            r"D:\photos\2024".to_owned(),
            r"E:\card".to_owned(),
        ];

        let warnings = destination_overlaps(&sources, r"D:\photos");
        assert_eq!(warnings.len(), 2, "both clashes are named: {warnings:?}");
        assert!(warnings[0].contains("same folder"), "{warnings:?}");
        assert!(warnings[1].contains("inside the destination"), "{warnings:?}");

        let refusal = super::overlap_refusal(&sources, r"D:\photos").expect("both refused");
        assert!(refusal.contains(r"D:\photos"), "{refusal}");
        assert!(refusal.contains(r"D:\photos\2024"), "{refusal}");
        assert_eq!(
            refusal.matches("the destination is the same folder").count(),
            1,
            "{refusal}"
        );

        let hint = super::overlap_refusal_hint(&sources, r"D:\photos").expect("greyed out");
        assert!(hint.starts_with("Greyed out:"), "{hint}");
        assert!(hint.contains(r"D:\photos\2024"), "{hint}");
    }

    #[test]
    fn several_picked_folders_fill_the_field_then_the_queue() {
        let mut app = ImporterApp::default();

        let (added, skipped) = app.add_sources([
            r"D:\one".to_owned(),
            r"E:\two".to_owned(),
            r"D:\ONE\".to_owned(),
            "   ".to_owned(),
        ]);

        assert_eq!((added, skipped), (2, 1), "the repeat and the blank drop out");
        assert_eq!(app.source, r"D:\one", "the first fills an empty field");
        assert_eq!(app.extra_sources, [r"E:\two"]);

        // A later pick queues behind the field, which is left alone, and the
        // folder already in the field is not queued again.
        let (added, skipped) = app.add_sources([r"F:\three".to_owned(), r"d:\one".to_owned()]);
        assert_eq!((added, skipped), (1, 1));
        assert_eq!(app.source, r"D:\one");
        assert_eq!(app.extra_sources, [r"E:\two", r"F:\three"]);
    }

    #[test]
    fn the_newest_log_button_names_its_program() {
        let mut app = ImporterApp::default();
        assert_eq!(app.editor_name(), "Notepad", "Notepad until one is chosen");
        assert!(app.editor().is_none());

        app.log_editor = r"C:\Program Files\Code\Code.exe".to_owned();
        assert_eq!(app.editor_name(), "Code");
        assert_eq!(
            app.editor().as_deref(),
            Some(std::path::Path::new(r"C:\Program Files\Code\Code.exe"))
        );

        // Blank means Notepad again, and a name-less path still shows something.
        app.log_editor = "   ".to_owned();
        assert_eq!(app.editor_name(), "Notepad");
        assert!(app.editor().is_none());
    }

    #[test]
    fn the_destination_offers_the_source_history_too() {
        let pinned = vec![r"F:\keep".to_owned()];
        let destinations = vec![r"E:\library".to_owned()];
        let sources = vec![
            r"D:\card".to_owned(),
            r"E:\LIBRARY".to_owned(),
            r"  ".to_owned(),
            r"F:\keep".to_owned(),
        ];

        assert_eq!(
            merged_recents(&destinations, &sources, &pinned),
            [r"F:\keep", r"E:\library", r"D:\card"],
            "own history first, repeats dropped, pinned ahead of everything"
        );

        // With only source history, the list is exactly that history.
        assert_eq!(
            merged_recents(&[], &sources, &[]),
            [r"D:\card", r"E:\LIBRARY", r"F:\keep"]
        );
        assert!(merged_recents(&[], &[], &[]).is_empty());
    }

    #[test]
    fn unpause_all_brings_every_paused_folder_back() {
        let mut app = ImporterApp::default();
        app.source = r"D:\card".to_owned();
        app.extra_sources = vec![r"E:\a".to_owned(), r"F:\b".to_owned()];
        app.set_disabled(r"E:\a", true);
        app.set_disabled(r"F:\b", true);
        assert_eq!(app.paused_folders().len(), 2);

        assert_eq!(app.unpause_all(), 2);
        assert!(app.paused_folders().is_empty());
        assert_eq!(app.status, "Resumed 2 paused source folders.");
        assert_eq!(
            app.source_folders(),
            [r"D:\card", r"E:\a", r"F:\b"],
            "the folders keep their places in the list"
        );

        // One folder reads as singular, and with nothing paused there is nothing
        // to say.
        app.set_disabled(r"F:\b", true);
        assert_eq!(app.unpause_all(), 1);
        assert_eq!(app.status, "Resumed 1 paused source folder.");

        let quiet = app.status.clone();
        assert_eq!(app.unpause_all(), 0);
        assert_eq!(app.status, quiet, "a no-op says nothing");
    }

    #[test]
    fn the_paused_note_counts_the_files_left_behind() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("card");
        std::fs::create_dir_all(folder.join("DCIM")).unwrap();
        std::fs::write(folder.join("DCIM").join("a.jpg"), b"jpeg").unwrap();
        std::fs::write(folder.join("DCIM").join("b.jpg"), b"jpeg").unwrap();
        std::fs::write(folder.join("notes.txt"), b"not a photo").unwrap();

        assert_eq!(folder_file_count(&folder), 2, "photos count, other files do not");

        let mut app = ImporterApp::default();
        app.source = r"D:\in".to_owned();
        let queued = folder.to_string_lossy().into_owned();
        app.extra_sources = vec![queued.clone()];
        app.set_disabled(&queued, true);

        app.finish(&summary_of(1), "Done");

        assert!(
            app.log
                .contains(&format!("paused: {} (2 files).", folder.display())),
            "{}",
            app.log
        );

        // A single file reads as singular, and a folder that has gone since it
        // was queued is named without a count.
        std::fs::remove_file(folder.join("DCIM").join("b.jpg")).unwrap();
        app.finish(&summary_of(1), "Done");
        assert!(
            app.log
                .contains(&format!("paused: {} (1 file).", folder.display())),
            "{}",
            app.log
        );

        std::fs::remove_dir_all(&folder).unwrap();
        app.finish(&summary_of(1), "Done");
        assert!(
            app.log
                .contains(&format!("paused: {}.", folder.display())),
            "{}",
            app.log
        );
    }

    #[test]
    fn the_paused_note_names_the_folders_it_skipped() {
        let mut app = ImporterApp::default();
        app.source = r"D:\card".to_owned();
        app.extra_sources = vec![r"E:\card2".to_owned(), r"F:\card3".to_owned()];
        app.set_disabled(r"E:\card2", true);
        app.set_disabled(r"F:\card3", true);

        assert_eq!(app.paused_folders(), [r"E:\card2", r"F:\card3"]);
        assert_eq!(app.paused_folders().len(), 2, "both paused folders are listed");

        app.finish(&summary_of(2), "Done");
        assert!(
            app.log.contains(
                "Skipped 2 source folders because they were paused: E:\\card2, F:\\card3."
            ),
            "{}",
            app.log
        );

        // One paused folder reads as singular, and names itself.
        app.set_disabled(r"F:\card3", false);
        app.finish(&summary_of(1), "Done");
        assert!(
            app.log
                .contains("Skipped 1 source folder because it was paused: E:\\card2."),
            "{}",
            app.log
        );
    }

    #[test]
    fn a_copied_but_undeleted_original_is_said_plainly() {
        let mut app = ImporterApp::default();
        app.source_root = PathBuf::from(r"D:\card");
        app.destination_root = PathBuf::from(r"E:\Library");

        app.handle_event(crate::importer::Event::LeftBehind {
            from: PathBuf::from(r"D:\card\IMG_0042.JPG"),
            to: PathBuf::from(r"E:\Library\2026\2026-06-01\IMG_0042.JPG"),
            reason: "Access is denied. (os error 5)".to_owned(),
        });

        assert!(
            app.log
                .contains("Copied IMG_0042.JPG to 2026\\2026-06-01\\IMG_0042.JPG"),
            "{}",
            app.log
        );
        assert!(
            app.log.contains(
                "but could not delete it from the source: Access is denied. (os error 5)"
            ),
            "{}",
            app.log
        );
        assert_eq!(app.failed, 0, "the copy is in the library, not a failed file");
    }

    #[test]
    fn several_dropped_photos_bring_each_of_their_folders() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let a = first.path().join("a.jpg");
        let b = first.path().join("b.jpg");
        let c = second.path().join("c.jpg");
        for photo in [&a, &b, &c] {
            std::fs::write(photo, b"jpeg").unwrap();
        }

        let folders = dropped_folders(&[a, b, c]);
        assert_eq!(folders.len(), 2, "one folder per place, repeats collapsed");
        assert_eq!(folders[0], first.path());
        assert_eq!(folders[1], second.path());

        // A dropped folder is itself, and a path that no longer exists brings
        // nothing at all.
        assert_eq!(
            dropped_folders(&[second.path().to_path_buf()]),
            vec![second.path().to_path_buf()]
        );
        let gone = first.path().join("gone").join("x.jpg");
        assert!(dropped_folders(&[gone]).is_empty());
        assert!(dropped_folders(&[]).is_empty());
    }

    #[test]
    fn a_typed_folder_is_remembered_without_a_run() {
        let mut app = ImporterApp::default();
        app.source = r"  D:\card  ".to_owned();
        app.destination = r#""E:\library""#.to_owned();

        app.remember_typed(false);
        app.remember_typed(true);

        assert_eq!(app.recent_sources, [r"D:\card"], "stored tidied, not as typed");
        assert_eq!(app.recent_destinations, [r"E:\library"]);

        // The same folder typed again moves to the front once, and an empty
        // field contributes nothing.
        app.source = r"D:\card".to_owned();
        app.remember_typed(false);
        assert_eq!(app.recent_sources.len(), 1);

        app.source = "   ".to_owned();
        app.remember_typed(false);
        assert_eq!(app.recent_sources.len(), 1);
        assert!(!app.running, "and no run was started");
    }

    #[test]
    fn the_bytes_copied_so_far_add_up_under_the_bar() {
        let mut app = ImporterApp::default();
        assert!(app.bytes_label().is_none(), "nothing copied yet");

        app.bytes = 2 * 1024 * 1024;
        assert_eq!(app.bytes_label().as_deref(), Some("2.0 MB copied so far"));

        app.dry_run = true;
        assert_eq!(app.bytes_label().as_deref(), Some("2.0 MB planned so far"));
    }

    #[test]
    fn refusing_an_overlapping_destination_stops_the_run() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().to_string_lossy().into_owned();

        let mut app = ImporterApp::default();
        app.source = folder.clone();
        app.destination = folder.clone();
        app.refuse_overlap = true;

        app.start();

        assert!(!app.running, "nothing should have started");
        let error = app.error.clone().expect("a refusal message");
        assert!(error.contains("same folder"), "{error}");
        assert!(
            error.contains("refusing an overlap like that is ticked"),
            "{error}"
        );

        // Start refuses a destination nested inside the source as well, and names
        // the folder it clashes with.
        let nested_source = directory.path().join("in");
        std::fs::create_dir_all(&nested_source).unwrap();
        app.source = nested_source.to_string_lossy().into_owned();
        app.destination = nested_source.join("library").to_string_lossy().into_owned();
        app.error = None;

        app.start();

        assert!(!app.running, "a nested overlap is refused too");
        let error = app.error.clone().expect("a nested refusal");
        assert!(error.contains("inside the source"), "{error}");

        // The other way round: a source folder inside the destination.
        let reversed = overlap_refusal(&[r"D:\photos\2024".to_owned()], r"D:\photos");
        assert!(
            reversed
                .as_deref()
                .unwrap_or_default()
                .contains("is inside the destination"),
            "{reversed:?}"
        );

        // A folder that only shares a name prefix is a different folder, as is any
        // unrelated one, and an empty list clashes with nothing.
        assert!(overlap_refusal(&[r"E:\card".to_owned()], r"E:\card2").is_none());
        assert!(overlap_refusal(&[r"E:\card".to_owned()], r"F:\library").is_none());
        assert!(overlap_refusal(&[], r"D:\card").is_none());

        // The same folder is still caught when case or a trailing slash differs.
        assert!(
            overlap_refusal(&[r"d:\CARD\".to_owned()], r"D:\card")
                .is_some_and(|text| text.contains("same folder"))
        );
    }

    #[test]
    fn a_path_field_says_so_when_its_folder_is_not_there() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().to_string_lossy().into_owned();
        let missing = directory.path().join("not-there");

        assert_eq!(missing_folder_note("", false), None, "an empty field is quiet");
        assert_eq!(
            missing_folder_note(&folder, false),
            None,
            "a folder that exists is fine"
        );
        assert_eq!(
            missing_folder_note(&missing.to_string_lossy(), false),
            Some(format!("No folder at {}", missing.display()))
        );

        // A destination may be created, so a missing folder below an existing one
        // is not a complaint.
        assert_eq!(
            missing_folder_note(&missing.to_string_lossy(), true),
            None,
            "the run would create it"
        );
    }

    #[test]
    fn a_click_on_the_missing_folder_warning_creates_the_folder() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("new-inbox");
        let text = missing.to_string_lossy().into_owned();

        let mut app = ImporterApp::default();
        app.create_missing_folder(&text);

        assert!(missing.is_dir(), "the folder is made");
        assert!(app.status.contains("Created"), "{}", app.status);
        assert!(app.log.contains(text.as_str()), "{}", app.log);
    }

    #[test]
    fn a_folder_that_cannot_be_made_says_why() {
        let directory = tempfile::tempdir().unwrap();
        // A file where a folder would have to go, so the folder cannot be made.
        let blocker = directory.path().join("taken");
        std::fs::write(&blocker, b"x").unwrap();
        let target = blocker.join("inside");

        let mut app = ImporterApp::default();
        app.create_missing_folder(&target.to_string_lossy());

        assert!(!target.is_dir(), "a file is in the way");
        assert!(
            app.error
                .as_deref()
                .is_some_and(|error| error.contains("Could not create")),
            "{:?}",
            app.error
        );
    }

    #[test]
    fn a_pasted_photo_path_becomes_the_folder_that_holds_it() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path();
        let photo = folder.join("IMG_0042.jpg");
        std::fs::write(&photo, b"jpeg").unwrap();

        assert_eq!(
            clean_folder(&format!("\"{}\"", photo.display())),
            folder.to_string_lossy(),
            "a pasted photo becomes its folder, quotes and all"
        );

        // A video that is not on disk yet still names a file by its extension.
        assert_eq!(clean_folder(r"E:\card\DCIM\CLIP.MOV"), r"E:\card\DCIM");

        // Folders, and paths that are not media at all, are left alone.
        assert_eq!(
            clean_folder(&folder.to_string_lossy()),
            folder.to_string_lossy()
        );
        assert_eq!(
            clean_folder(r"D:\notes\readme.txt"),
            r"D:\notes\readme.txt"
        );
        assert_eq!(clean_folder("IMG_0042.jpg"), "IMG_0042.jpg");
    }

    #[test]
    fn pasted_paths_lose_their_quotes_and_spaces() {
        assert_eq!(
            clean_folder(r#""D:\My Photos\DCIM""#),
            r"D:\My Photos\DCIM",
            "Explorer's Copy as path arrives quoted"
        );
        assert_eq!(clean_folder("  D:\\Photos  "), r"D:\Photos");
        assert_eq!(clean_folder("D:\\Photos\n"), r"D:\Photos");
        assert_eq!(
            clean_folder(r#""D:\a"#),
            r#""D:\a"#,
            "a lone quote is left alone"
        );
        assert_eq!(clean_folder(""), "");
    }

    #[test]
    fn right_clicking_a_recent_entry_forgets_it_and_its_pin() {
        let mut list = vec![r"D:\a".to_owned(), r"D:\b".to_owned()];
        let mut pinned = vec![r"D:\a".to_owned()];

        forget_recent(&mut list, &mut pinned, r"d:\A");

        assert_eq!(list, [r"D:\b"], "only the clicked folder goes");
        assert!(pinned.is_empty(), "it should not come back pinned later");
    }

    #[test]
    fn pinning_moves_a_folder_to_the_top_of_the_recents() {
        let mut app = ImporterApp::default();
        app.recent_sources = vec![r"D:\b".to_owned(), r"D:\a".to_owned()];
        app.recent_destinations = vec![r"E:\out".to_owned(), r"D:\a".to_owned()];

        app.set_pinned(r"D:\A", true);

        assert_eq!(app.recent_sources, [r"D:\a", r"D:\b"]);
        assert_eq!(app.recent_destinations, [r"D:\a", r"E:\out"]);
        assert!(app.is_pinned(r"d:\a"));

        app.set_pinned(r"D:\a", false);
        assert!(!app.is_pinned(r"D:\a"));
        assert_eq!(app.recent_sources, [r"D:\a", r"D:\b"], "position is kept");
    }

    #[test]
    fn the_free_space_label_names_the_destination_drive() {
        let mut app = ImporterApp::default();
        assert!(app.free_space_label().is_none(), "nothing measured yet");

        app.destination = r"E:\library".to_owned();
        app.free_space = Some(500 * 1024 * 1024 * 1024);
        assert_eq!(app.free_space_label().as_deref(), Some("E: 500.0 GB free"));

        // A real folder gives a real reading.
        let directory = tempfile::tempdir().unwrap();
        app.destination = directory.path().to_string_lossy().into_owned();
        app.refresh_free_space();
        assert!(
            app.free_space.is_some_and(|free| free > 0),
            "{:?}",
            app.free_space
        );
    }

    #[test]
    fn the_space_check_waits_for_a_real_folder_and_a_real_run() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = ImporterApp::default();
        app.destination = directory.path().join("not-created").to_string_lossy().into_owned();

        app.refresh_warnings();
        assert!(app.warnings.is_empty(), "a path that is not a folder yet");

        // A picked folder is measured straight away, but a dry run copies
        // nothing, so it never earns a space warning.
        app.destination = directory.path().to_string_lossy().into_owned();
        app.dry_run = true;
        app.refresh_warnings();
        assert!(app.warnings.is_empty(), "a dry run copies nothing");

        app.dry_run = false;
        app.refresh_warnings();
        assert!(
            app.warnings.is_empty(),
            "an empty source folder has nothing to weigh: {:?}",
            app.warnings
        );
    }

    #[test]
    fn a_run_with_every_folder_paused_says_so_plainly() {
        let mut app = ImporterApp::default();
        app.destination = r"E:\library".to_owned();
        app.extra_sources = vec![r"D:\card".to_owned()];
        app.set_disabled(r"D:\card", true);

        let message = app.all_paused_error().expect("a plain explanation");
        assert!(
            message.starts_with("Nothing to import: the only source folder is paused"),
            "{message}"
        );
        assert!(
            message.contains(r"D:\card"),
            "the paused folder is named: {message}"
        );

        app.extra_sources = vec![r"D:\card".to_owned(), r"E:\card2".to_owned()];
        app.set_disabled(r"E:\card2", true);
        let message = app.all_paused_error().expect("a plain explanation");
        assert!(message.contains("all 2 source folders are paused"), "{message}");
        assert!(
            message.contains(r"D:\card, E:\card2"),
            "both folders are named in queue order: {message}"
        );

        // A folder in the field is something to import, whatever the queue says.
        app.source = r"F:\in".to_owned();
        assert!(app.all_paused_error().is_none());

        // Partly paused: the rest still runs, so there is nothing to explain.
        let mut partly = ImporterApp::default();
        partly.extra_sources = vec![r"D:\a".to_owned(), r"E:\b".to_owned()];
        partly.set_disabled(r"D:\a", true);
        assert!(partly.all_paused_error().is_none());

        // Nothing queued at all: the ordinary "pick a folder" message stands.
        assert!(ImporterApp::default().all_paused_error().is_none());
    }

    #[test]
    fn the_main_source_can_be_dragged_into_the_queue() {
        let mut app = ImporterApp::default();
        app.source = r"D:\card".to_owned();
        app.extra_sources = vec![r"E:\one".to_owned(), r"F:\two".to_owned()];

        assert!(app.demote_main_source(1));
        assert_eq!(app.source, r"E:\one");
        assert_eq!(app.extra_sources, [r"D:\card", r"F:\two"]);
        assert_eq!(
            app.source_folders(),
            [r"E:\one", r"D:\card", r"F:\two"],
            "the visible order is unchanged, only which folder heads it"
        );

        // Dropping at the very top changes nothing.
        assert!(!app.demote_main_source(0));

        // With nothing queued there is nowhere to put it.
        let mut lonely = ImporterApp::default();
        lonely.source = r"D:\card".to_owned();
        assert!(!lonely.demote_main_source(0));
    }

    #[test]
    fn a_paused_folder_promoted_to_the_field_switches_on() {
        let mut app = ImporterApp::default();
        app.source = r"D:\card".to_owned();
        app.extra_sources = vec![r"E:\paused".to_owned(), r"F:\two".to_owned()];
        app.set_disabled(r"E:\paused", true);

        assert!(app.demote_main_source(1));

        assert_eq!(app.source, r"E:\paused");
        assert!(!app.is_disabled(r"E:\paused"));
        assert_eq!(app.source_folders(), [r"E:\paused", r"D:\card", r"F:\two"]);
    }

    #[test]
    fn finishing_a_run_says_how_many_folders_were_paused() {
        let mut app = ImporterApp::default();
        app.source = r"D:\in".to_owned();
        app.extra_sources = vec![r"E:\card".to_owned(), r"F:\card2".to_owned()];
        app.set_disabled(r"E:\card", true);

        app.finish(&summary_of(2), "Done");

        assert!(
            app.log
                .contains("Skipped 1 source folder because it was paused: E:\\card."),
            "{}",
            app.log
        );
        assert!(
            app.last_summary
                .as_deref()
                .unwrap_or_default()
                .contains("Skipped 1 source folder because it was paused: E:\\card."),
            "the copied summary carries the note too"
        );

        // Two paused folders read as plural.
        app.set_disabled(r"F:\card2", true);
        app.finish(&summary_of(2), "Done");
        assert!(
            app.log
                .contains("Skipped 2 source folders because they were paused: "),
            "{}",
            app.log
        );

        // With nothing paused there is no line at all.
        let mut quiet = ImporterApp::default();
        quiet.finish(&summary_of(1), "Done");
        assert!(
            !quiet.log.contains("paused"),
            "unexpected note: {}",
            quiet.log
        );
    }

    #[test]
    fn pausing_a_queued_folder_skips_it_without_removing_it() {
        let mut app = ImporterApp::default();
        app.source = r"D:\in".to_owned();
        app.extra_sources = vec![r"E:\card".to_owned(), r"F:\card2".to_owned()];

        assert_eq!(app.source_folders(), [r"D:\in", r"E:\card", r"F:\card2"]);

        app.set_disabled(r"E:\card", true);
        assert_eq!(app.source_folders(), [r"D:\in", r"F:\card2"]);
        assert_eq!(app.extra_sources.len(), 2, "the folder stays in the list");

        // Matching is case-insensitive, like the rest of the folder handling.
        app.set_disabled(r"e:\CARD", false);
        assert_eq!(app.source_folders(), [r"D:\in", r"E:\card", r"F:\card2"]);
        assert!(app.disabled_sources.is_empty());
    }

    #[test]
    fn queued_folders_can_be_moved_up_and_down() {
        let mut folders = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];

        assert!(move_item(&mut folders, 0, 2));
        assert_eq!(folders, ["b", "c", "a"]);

        assert!(move_item(&mut folders, 2, 0));
        assert_eq!(folders, ["a", "b", "c"]);

        assert!(move_item(&mut folders, 0, 1));
        assert_eq!(folders, ["b", "a", "c"]);

        // Dropping a folder back on itself, or off the end, changes nothing.
        assert!(!move_item(&mut folders, 1, 1));
        assert!(!move_item(&mut folders, 5, 0));
        assert_eq!(folders, ["b", "a", "c"]);
    }

    #[test]
    fn the_select_argument_keeps_the_whole_path_together() {
        assert_eq!(
            select_arg(Path::new(r"E:\Library\import-log-2026-11-06_204512.txt")),
            r#"/select,"E:\Library\import-log-2026-11-06_204512.txt""#
        );
        assert_eq!(
            select_arg(Path::new(r"E:\My Photos\import-log-2026-11-06_204512.txt")),
            r#"/select,"E:\My Photos\import-log-2026-11-06_204512.txt""#
        );
    }

    #[test]
    fn the_summary_line_reports_the_total_size() {
        let mut app = ImporterApp::default();
        let mut summary = summary_of(1);
        summary.bytes = 3 * 1024 * 1024;

        app.finish(&summary, "Done");

        assert!(
            app.log.contains("Done: 1 imported (3.0 MB)"),
            "missing the total size: {}",
            app.log
        );
    }

    #[test]
    fn the_newest_run_log_is_found_and_other_files_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path();

        assert!(newest_run_log(folder).is_none(), "an empty destination");

        for name in [
            "import-log-2026-01-01_090000.txt",
            "import-log-2026-06-01_101112.txt",
            "import-log-2026-06-01_101112_2.txt",
            "notes.txt",
            "IMG_0001.JPG",
        ] {
            std::fs::write(folder.join(name), "x").unwrap();
        }

        let newest = newest_run_log(folder).expect("a log to open");
        assert_eq!(
            newest.file_name().map(|name| name.to_string_lossy().into_owned()),
            Some("import-log-2026-06-01_101112_2.txt".to_owned()),
            "the same-second suffix counts as newer"
        );
    }

    #[test]
    fn the_log_count_is_clamped_for_the_run() {
        let mut app = ImporterApp::default();

        app.logs_to_keep = 0;
        assert_eq!(app.kept_logs(), super::DEFAULT_LOGS_TO_KEEP);

        app.logs_to_keep = 5;
        assert_eq!(app.kept_logs(), 5);

        app.logs_to_keep = 10_000;
        assert_eq!(app.kept_logs(), super::MAX_LOGS_TO_KEEP);
    }

    #[test]
    fn changing_the_keep_number_prunes_at_once() {
        let directory = tempfile::tempdir().unwrap();

        for minute in 0..8 {
            std::fs::write(
                directory
                    .path()
                    .join(format!("import-log-2026-01-01_1200{minute:02}.txt")),
                "old run",
            )
            .unwrap();
        }
        std::fs::write(directory.path().join("notes.txt"), "keep me").unwrap();

        let mut app = ImporterApp::default();
        app.destination = directory.path().to_string_lossy().into_owned();
        app.logs_to_keep = 8;
        app.set_log_limit(2);

        let remaining: Vec<String> = std::fs::read_dir(directory.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| is_run_log(Path::new(name)))
            .collect();

        assert_eq!(
            remaining,
            [
                "import-log-2026-01-01_120006.txt",
                "import-log-2026-01-01_120007.txt"
            ],
            "only the newest few should be left"
        );
        assert_eq!(app.logs_to_keep, 2);
        assert!(directory.path().join("notes.txt").is_file());
        assert!(
            app.status.contains("deleted 6 older"),
            "{}",
            app.status
        );
        assert!(app.log.contains("Logs: keeping the newest 2, removed 6 older"));
    }

    #[test]
    fn delete_logs_removes_every_run_log_but_nothing_else() {
        let directory = tempfile::tempdir().unwrap();
        let (first, first_path) = create_run_log(directory.path(), "2026-11-06_204512", 20).unwrap();
        let (second, second_path) =
            create_run_log(directory.path(), "2026-11-06_204513", 20).unwrap();
        drop((first, second));
        std::fs::write(directory.path().join("notes.txt"), "keep me").unwrap();

        let mut app = ImporterApp::default();
        app.destination = directory.path().to_string_lossy().into_owned();
        app.delete_logs();

        assert!(!first_path.exists(), "first log should be gone");
        assert!(!second_path.exists(), "second log should be gone");
        assert!(directory.path().join("notes.txt").is_file());
        assert!(
            app.status.contains("Deleted 2 run log(s)"),
            "{}",
            app.status
        );
        assert!(app.log.contains("Delete logs: removed 2 run log(s)"));
    }

    #[test]
    fn only_the_newest_run_logs_are_kept() {
        let directory = tempfile::tempdir().unwrap();

        for minute in 0..8 {
            std::fs::write(
                directory
                    .path()
                    .join(format!("import-log-2026-01-01_1200{minute:02}.txt")),
                "old run",
            )
            .unwrap();
        }
        // Anything that is not a run log belongs to the user and must survive.
        std::fs::write(directory.path().join("notes.txt"), "keep me").unwrap();

        let (file, new_log) =
            create_run_log(directory.path(), "2026-11-06_204512", 5).unwrap();
        drop(file);

        let remaining: Vec<String> = std::fs::read_dir(directory.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| is_run_log(Path::new(name)))
            .collect();

        assert_eq!(remaining.len(), 5, "the requested count should be kept");
        assert!(remaining.contains(&new_log.file_name().unwrap().to_string_lossy().into_owned()));
        assert!(directory.path().join("notes.txt").is_file());
        assert!(
            !directory
                .path()
                .join("import-log-2026-01-01_120000.txt")
                .exists(),
            "the oldest log should have been pruned"
        );
    }

    #[test]
    fn log_lines_are_streamed_to_the_run_file() {
        let directory = tempfile::tempdir().unwrap();
        let (file, path) = create_run_log(directory.path(), "2026-11-06_204512", 20).unwrap();

        let mut app = ImporterApp::default();
        app.log_file = Some(file);
        app.push_log("first line");
        app.push_log("second line");
        app.close_log_file();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, "first line\nsecond line\n");
    }
}
