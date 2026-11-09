//! Remembering the folders, mode and window placement used last time the app ran.
//!
//! The settings live in `%APPDATA%\ImageImporter\settings.txt` as a tiny
//! `key=value` file, so no extra dependencies or registry writes are needed.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::importer::TransferMode;

/// Window size used when nothing usable was stored.
pub const DEFAULT_WINDOW_SIZE: (f32, f32) = (780.0, 700.0);
/// Smallest window the app allows, matching the minimum enforced on the window.
pub const MIN_WINDOW_SIZE: (f32, f32) = (620.0, 520.0);
/// Beyond this, a stored placement is treated as stale rather than real.
const MAX_WINDOW_EXTENT: f32 = 20_000.0;
/// Run logs kept in the destination when nothing else is stored.
pub const DEFAULT_LOGS_TO_KEEP: usize = 5;
/// At least one log is kept, otherwise the run's own log would vanish.
pub const MIN_LOGS_TO_KEEP: usize = 1;
/// Enough for a busy library without the folder filling up.
pub const MAX_LOGS_TO_KEEP: usize = 999;

/// State that survives a restart of the app.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub source: String,
    /// Extra source folders queued alongside `source`, in order.
    pub extra_sources: Vec<String>,
    /// Queued folders that are paused: kept in place but not imported.
    pub disabled_sources: Vec<String>,
    /// Favourite folders: listed first in the recent lists and never dropped.
    pub pinned: Vec<String>,
    pub destination: String,
    /// Copy is the default, so only Move needs a line in the file.
    pub mode: TransferMode,
    /// Whether the Dry run box is ticked, so a careful session reopens careful.
    pub dry_run: bool,
    /// How many run logs the destination keeps.
    pub logs_to_keep: usize,
    /// Whether a run is refused when the destination and a source overlap.
    pub refuse_overlap: bool,
    /// Program that opens run logs; empty means Notepad.
    pub log_editor: String,
    /// Window size in logical points, as reported by the window.
    pub window_size: Option<(f32, f32)>,
    /// Window top-left corner in logical points.
    pub window_position: Option<(f32, f32)>,
    /// Whether the window was maximized, so it reopens the same way.
    pub maximized: bool,
    /// Source folders used recently, most recent first.
    pub recent_sources: Vec<String>,
    /// Destination folders used recently, most recent first.
    pub recent_destinations: Vec<String>,
}

/// How many folders each recent list keeps.
pub const MAX_RECENT: usize = 8;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            source: String::new(),
            extra_sources: Vec::new(),
            disabled_sources: Vec::new(),
            pinned: Vec::new(),
            destination: String::new(),
            mode: TransferMode::Copy,
            dry_run: false,
            logs_to_keep: DEFAULT_LOGS_TO_KEEP,
            refuse_overlap: false,
            log_editor: String::new(),
            window_size: None,
            window_position: None,
            maximized: false,
            recent_sources: Vec::new(),
            recent_destinations: Vec::new(),
        }
    }
}

impl Settings {
    /// Load the stored settings, falling back to empty fields.
    pub fn load() -> Self {
        match Self::location() {
            Some(path) => Self::load_from(&path).unwrap_or_default(),
            None => Self::default(),
        }
    }

    /// Persist the settings. Remembering folders is a convenience, so a failure
    /// (read-only profile, full disk) is deliberately ignored rather than
    /// interrupting an import.
    pub fn save(&self) {
        if let Some(path) = Self::location() {
            let _ = Self::save_to(&path, self);
        }
    }

    /// Forget the remembered folders and mode, as the Reset button does. The
    /// window placement is kept, so the app still opens where it was left.
    pub fn clear() {
        Self::load().cleared_of_folders().save();
    }

    /// These settings with the folders, mode and recent lists forgotten.
    pub fn cleared_of_folders(&self) -> Self {
        Settings {
            source: String::new(),
            extra_sources: Vec::new(),
            disabled_sources: Vec::new(),
            pinned: self.pinned.clone(),
            destination: String::new(),
            mode: TransferMode::Copy,
            logs_to_keep: self.logs_to_keep,
            // A reset forgets folders, not how careful the user wants to be, nor
            // which program they open logs with.
            dry_run: self.dry_run,
            refuse_overlap: self.refuse_overlap,
            log_editor: self.log_editor.clone(),
            window_size: self.window_size,
            window_position: self.window_position,
            maximized: self.maximized,
            recent_sources: Vec::new(),
            recent_destinations: Vec::new(),
        }
    }

    /// Move `folder` to the front of a recent list, dropping duplicates
    /// (Windows paths compare case-insensitively) and keeping the list short.
    ///
    /// Pinned folders move to the front of the list and are never dropped, so
    /// the count can grow past `MAX_RECENT` when favourites stack up.
    pub fn remember(history: &mut Vec<String>, folder: &str) {
        Self::remember_with(history, folder, &[]);
    }

    /// [`Settings::remember`] with pinned favourites: they float to the front of
    /// the list and are never dropped to make room.
    pub fn remember_with(history: &mut Vec<String>, folder: &str, pinned: &[String]) {
        let folder = folder.trim();
        if folder.is_empty() {
            return;
        }

        history.retain(|entry| !entry.eq_ignore_ascii_case(folder));
        history.insert(0, folder.to_owned());
        Self::pinned_first(history, pinned);

        // The oldest entries beyond the cap go, but a pinned folder stays put.
        while history.len() > MAX_RECENT {
            let Some(index) = history
                .iter()
                .rposition(|entry| !is_pinned(pinned, entry))
            else {
                break;
            };
            history.remove(index);
        }
    }

    /// Put the pinned folders at the front, keeping each group's own order.
    pub fn pinned_first(history: &mut Vec<String>, pinned: &[String]) {
        let (front, rest): (Vec<String>, Vec<String>) = history
            .drain(..)
            .partition(|entry| is_pinned(pinned, entry));

        history.extend(front);
        history.extend(rest);
    }

    /// Pin or unpin a folder as a favourite.
    pub fn set_pinned(pinned: &mut Vec<String>, folder: &str, on: bool) {
        let folder = folder.trim();
        if folder.is_empty() {
            return;
        }

        pinned.retain(|entry| !entry.eq_ignore_ascii_case(folder));
        if on {
            pinned.push(folder.to_owned());
        }
    }

    /// The window size to open with, ignoring a value that makes no sense.
    pub fn usable_window_size(&self) -> (f32, f32) {
        match self.window_size {
            Some((width, height))
                if width >= MIN_WINDOW_SIZE.0
                    && height >= MIN_WINDOW_SIZE.1
                    && width <= MAX_WINDOW_EXTENT
                    && height <= MAX_WINDOW_EXTENT =>
            {
                (width, height)
            }
            _ => DEFAULT_WINDOW_SIZE,
        }
    }

    /// The window position to open at, ignoring a placement that would put the
    /// window somewhere unreachable, such as a monitor that has been removed.
    pub fn usable_window_position(&self) -> Option<(f32, f32)> {
        let (x, y) = self.window_position?;
        let reachable = x.abs() <= MAX_WINDOW_EXTENT && y.abs() <= MAX_WINDOW_EXTENT;
        reachable.then_some((x, y))
    }

    /// `%APPDATA%\ImageImporter\settings.txt`
    fn location() -> Option<PathBuf> {
        // A test run keeps to its own files, so it can never read - or overwrite -
        // the settings of the person running it.
        if cfg!(test) {
            return None;
        }

        let appdata = std::env::var_os("APPDATA")?;
        Some(
            PathBuf::from(appdata)
                .join("ImageImporter")
                .join("settings.txt"),
        )
    }

    pub fn load_from(path: &Path) -> Option<Self> {
        let text = fs::read_to_string(path).ok()?;
        let mut settings = Settings::default();

        for line in text.lines() {
            // Unknown or malformed lines are skipped so an edited file cannot
            // stop the app from starting.
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.strip_suffix('\r').unwrap_or(value);
            match key.trim() {
                "source" => settings.source = value.to_owned(),
                "source_extra" => push_recent(&mut settings.extra_sources, value),
                "source_extra_off" => push_recent(&mut settings.disabled_sources, value),
                "pinned" => push_recent(&mut settings.pinned, value),
                "destination" => settings.destination = value.to_owned(),
                "mode" => settings.mode = parse_mode(value),
                "dry_run" => settings.dry_run = parse_bool(value),
                "logs_to_keep" => settings.logs_to_keep = parse_logs_to_keep(value),
                "refuse_overlap" => settings.refuse_overlap = parse_bool(value),
                "log_editor" => settings.log_editor = value.trim().to_owned(),
                "window_size" => settings.window_size = parse_pair(value, 'x'),
                "window_position" => settings.window_position = parse_pair(value, ','),
                "maximized" => settings.maximized = parse_bool(value),
                "recent_source" => push_recent(&mut settings.recent_sources, value),
                "recent_destination" => push_recent(&mut settings.recent_destinations, value),
                _ => {}
            }
        }

        Some(settings)
    }

    pub fn save_to(path: &Path, settings: &Settings) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut text = String::new();
        if !settings.source.is_empty() {
            text.push_str("source=");
            text.push_str(&settings.source);
            text.push('\n');
        }
        for folder in &settings.extra_sources {
            text.push_str(&format!("source_extra={folder}\n"));
        }
        for folder in &settings.disabled_sources {
            text.push_str(&format!("source_extra_off={folder}\n"));
        }
        for folder in &settings.pinned {
            text.push_str(&format!("pinned={folder}\n"));
        }
        if !settings.destination.is_empty() {
            text.push_str("destination=");
            text.push_str(&settings.destination);
            text.push('\n');
        }
        if settings.mode == TransferMode::Move {
            text.push_str("mode=move\n");
        }
        if settings.dry_run {
            text.push_str("dry_run=1\n");
        }
        if settings.logs_to_keep != DEFAULT_LOGS_TO_KEEP {
            text.push_str(&format!("logs_to_keep={}\n", settings.logs_to_keep));
        }
        if settings.refuse_overlap {
            text.push_str("refuse_overlap=1\n");
        }
        if !settings.log_editor.trim().is_empty() {
            text.push_str(&format!("log_editor={}\n", settings.log_editor.trim()));
        }
        if let Some((width, height)) = settings.window_size {
            text.push_str(&format!("window_size={width:.0}x{height:.0}\n"));
        }
        if let Some((x, y)) = settings.window_position {
            text.push_str(&format!("window_position={x:.0},{y:.0}\n"));
        }
        if settings.maximized {
            text.push_str("maximized=1\n");
        }
        for folder in &settings.recent_sources {
            text.push_str(&format!("recent_source={folder}\n"));
        }
        for folder in &settings.recent_destinations {
            text.push_str(&format!("recent_destination={folder}\n"));
        }

        fs::write(path, text)
    }
}

/// A count that makes no sense falls back to the default, and a wild one is
/// clamped so a typo in the file cannot keep thousands of logs or none.
fn parse_logs_to_keep(value: &str) -> usize {
    value
        .trim()
        .parse::<usize>()
        .map(|count| count.clamp(MIN_LOGS_TO_KEEP, MAX_LOGS_TO_KEEP))
        .unwrap_or(DEFAULT_LOGS_TO_KEEP)
}

/// Unknown or missing values fall back to the safe default, Copy.
fn parse_mode(value: &str) -> TransferMode {
    match value.trim().to_ascii_lowercase().as_str() {
        "move" => TransferMode::Move,
        _ => TransferMode::Copy,
    }
}

/// A flag that is only on when the file says so clearly; anything else means the
/// window opens at its normal size.
fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Whether `folder` is one of the pinned favourites.
pub fn is_pinned(pinned: &[String], folder: &str) -> bool {
    let folder = folder.trim();
    pinned.iter().any(|entry| entry.eq_ignore_ascii_case(folder))
}

/// Add a stored history entry, ignoring blanks and repeats.
fn push_recent(history: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if value.is_empty() || history.iter().any(|entry| entry.eq_ignore_ascii_case(value)) {
        return;
    }

    history.push(value.to_owned());
}

/// Parse a pair such as `1280x800` or `120,80`; anything else yields `None`.
fn parse_pair(value: &str, separator: char) -> Option<(f32, f32)> {
    let (left, right) = value.trim().split_once(separator)?;
    let left: f32 = left.trim().parse().ok()?;
    let right: f32 = right.trim().parse().ok()?;

    (left.is_finite() && right.is_finite()).then_some((left, right))
}

#[cfg(test)]
mod tests {
    use super::Settings;

    #[test]
    fn pinned_folders_float_to_the_top_and_never_drop_off() {
        let pinned = vec![r"D:\keep".to_owned()];
        let mut history = Vec::new();

        for index in 0..12 {
            super::Settings::remember_with(&mut history, &format!(r"E:\other{index}"), &pinned);
        }
        super::Settings::remember_with(&mut history, r"D:\keep", &pinned);

        assert_eq!(history.len(), super::MAX_RECENT);
        assert_eq!(history[0], r"D:\keep");

        // More folders than the cap later on: the favourite is still there.
        for index in 0..12 {
            super::Settings::remember_with(&mut history, &format!(r"F:\later{index}"), &pinned);
        }

        assert_eq!(history.len(), super::MAX_RECENT);
        assert_eq!(history[0], r"D:\keep", "a pinned folder is never dropped");
    }

    #[test]
    fn pinned_folders_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");

        let settings = Settings {
            recent_sources: vec![r"D:\keep".to_owned(), r"E:\other".to_owned()],
            pinned: vec![r"D:\keep".to_owned()],
            ..Settings::default()
        };
        Settings::save_to(&path, &settings).unwrap();
        let loaded = Settings::load_from(&path).unwrap();

        assert_eq!(loaded.recent_sources, [r"D:\keep", r"E:\other"]);
        assert_eq!(loaded.pinned, [r"D:\keep"]);
        assert!(super::is_pinned(&loaded.pinned, r"d:\KEEP"));
    }

    #[test]
    fn paused_sources_keep_their_place_in_the_queue() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");

        let settings = Settings {
            extra_sources: vec![r"D:\one".to_owned(), r"E:\two".to_owned()],
            disabled_sources: vec![r"E:\two".to_owned()],
            ..Settings::default()
        };
        Settings::save_to(&path, &settings).unwrap();
        let loaded = Settings::load_from(&path).unwrap();

        assert_eq!(loaded.extra_sources, [r"D:\one", r"E:\two"]);
        assert_eq!(loaded.disabled_sources, [r"E:\two"]);
    }

    #[test]
    fn tests_never_touch_the_real_settings_file() {
        assert!(
            Settings::location().is_none(),
            "unit tests must keep to their own files"
        );
    }

    #[test]
    fn the_dry_run_box_is_remembered() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");

        let settings = Settings {
            dry_run: true,
            ..Settings::default()
        };
        Settings::save_to(&path, &settings).unwrap();
        assert!(Settings::load_from(&path).unwrap().dry_run);

        // The line only appears when the box is ticked, so an ordinary session
        // leaves the file exactly as it was before.
        std::fs::write(&path, "source=D:\\in\n").unwrap();
        assert!(!Settings::load_from(&path).unwrap().dry_run);

        // A reset keeps the careful setting, as it keeps the other safety ones.
        assert!(settings.cleared_of_folders().dry_run);
    }

    #[test]
    fn logs_to_keep_round_trips_and_is_clamped() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");

        let settings = Settings {
            logs_to_keep: 7,
            ..Settings::default()
        };
        Settings::save_to(&path, &settings).unwrap();
        assert_eq!(Settings::load_from(&path).unwrap().logs_to_keep, 7);

        // A typo in the file cannot keep thousands of logs, or none at all.
        std::fs::write(&path, "logs_to_keep=999999\n").unwrap();
        assert_eq!(
            Settings::load_from(&path).unwrap().logs_to_keep,
            super::MAX_LOGS_TO_KEEP
        );
        std::fs::write(&path, "logs_to_keep=0\n").unwrap();
        assert_eq!(
            Settings::load_from(&path).unwrap().logs_to_keep,
            super::MIN_LOGS_TO_KEEP
        );
        std::fs::write(&path, "logs_to_keep=lots\n").unwrap();
        assert_eq!(
            Settings::load_from(&path).unwrap().logs_to_keep,
            super::DEFAULT_LOGS_TO_KEEP
        );
    }
    use crate::importer::TransferMode;
    use std::fs;
    use std::path::Path;

    #[test]
    fn round_trips_both_folders() {
        let directory = tempfile::tempdir().unwrap();
        // Also proves the parent folder gets created.
        let path = directory.path().join("nested").join("settings.txt");
        let settings = Settings {
            source: r"D:\Photos 2024\DCIM".to_owned(),
            destination: r"E:\Library".to_owned(),
            mode: TransferMode::Move,
            ..Settings::default()
        };

        Settings::save_to(&path, &settings).unwrap();

        assert_eq!(Settings::load_from(&path).unwrap(), settings);
        assert_eq!(
            Settings::load_from(&path).unwrap().mode,
            TransferMode::Move
        );
    }

    #[test]
    fn copy_mode_is_not_written_to_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");
        let settings = Settings {
            source: r"D:\in".to_owned(),
            destination: r"E:\out".to_owned(),
            mode: TransferMode::Copy,
            ..Settings::default()
        };

        Settings::save_to(&path, &settings).unwrap();
        let text = fs::read_to_string(&path).unwrap();

        assert!(
            !text.contains("mode"),
            "copy is the default and should not be written: {text}"
        );
        assert_eq!(Settings::load_from(&path).unwrap(), settings);
    }

    #[test]
    fn keeps_paths_containing_equals_signs() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");
        let settings = Settings {
            source: r"D:\a=b\c".to_owned(),
            destination: String::new(),
            mode: TransferMode::Copy,
            ..Settings::default()
        };

        Settings::save_to(&path, &settings).unwrap();
        let loaded = Settings::load_from(&path).unwrap();

        assert_eq!(loaded.source, r"D:\a=b\c");
        assert_eq!(loaded.destination, "");
        assert_eq!(loaded.mode, TransferMode::Copy);
    }

    #[test]
    fn missing_mode_line_means_copy() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");
        fs::write(&path, "source=D:\\\\x\ndestination=E:\\\\y\n").unwrap();

        assert_eq!(
            Settings::load_from(&path).unwrap().mode,
            TransferMode::Copy
        );
    }

    #[test]
    fn unknown_mode_falls_back_to_copy() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");
        fs::write(&path, "mode=sideways\n").unwrap();

        assert_eq!(
            Settings::load_from(&path).unwrap().mode,
            TransferMode::Copy
        );
    }

    #[test]
    fn window_placement_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");
        let settings = Settings {
            source: String::new(),
            destination: String::new(),
            mode: TransferMode::Copy,
            window_size: Some((1280.0, 800.0)),
            window_position: Some((120.0, 80.0)),
            maximized: false,
            recent_sources: Vec::new(),
            recent_destinations: Vec::new(),
            extra_sources: Vec::new(),
            disabled_sources: Vec::new(),
            pinned: Vec::new(),
            logs_to_keep: 20,
            dry_run: false,
            refuse_overlap: false,
            log_editor: String::new(),
        };

        Settings::save_to(&path, &settings).unwrap();
        let loaded = Settings::load_from(&path).unwrap();

        assert_eq!(loaded.usable_window_size(), (1280.0, 800.0));
        assert_eq!(loaded.usable_window_position(), Some((120.0, 80.0)));
    }

    #[test]
    fn maximized_state_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");

        assert!(!Settings::default().maximized, "normal size by default");

        let settings = Settings {
            maximized: true,
            ..Settings::default()
        };
        Settings::save_to(&path, &settings).unwrap();

        assert!(Settings::load_from(&path).unwrap().maximized);
        assert!(
            fs::read_to_string(&path).unwrap().contains("maximized=1"),
            "written only when maximized"
        );

        // Anything that is not a clear yes leaves the window normal-sized.
        fs::write(&path, "maximized=maybe\n").unwrap();
        assert!(!Settings::load_from(&path).unwrap().maximized);
    }

    #[test]
    fn junk_window_values_are_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");
        fs::write(&path, "window_size=bigxsmall\nwindow_position=,\n").unwrap();

        let loaded = Settings::load_from(&path).unwrap();

        assert!(loaded.window_size.is_none());
        assert!(loaded.window_position.is_none());
        assert_eq!(loaded.usable_window_size(), super::DEFAULT_WINDOW_SIZE);
        assert_eq!(loaded.usable_window_position(), None);
    }

    #[test]
    fn unusable_placement_falls_back_to_defaults() {
        let tiny = Settings {
            window_size: Some((10.0, 10.0)),
            ..Settings::default()
        };
        assert_eq!(tiny.usable_window_size(), super::DEFAULT_WINDOW_SIZE);

        // A window parked on a monitor that is no longer attached.
        let stranded = Settings {
            window_position: Some((-90_000.0, 40.0)),
            ..Settings::default()
        };
        assert_eq!(stranded.usable_window_position(), None);
    }

    #[test]
    fn reset_keeps_the_window_placement() {
        let settings = Settings {
            source: r"D:\in".to_owned(),
            destination: r"E:\out".to_owned(),
            mode: TransferMode::Move,
            window_size: Some((1000.0, 700.0)),
            window_position: Some((50.0, 60.0)),
            maximized: true,
            recent_sources: vec![r"D:\in".to_owned()],
            recent_destinations: vec![r"E:\out".to_owned()],
            extra_sources: vec![r"F:\card".to_owned()],
            disabled_sources: vec![r"F:\card".to_owned()],
            pinned: Vec::new(),
            logs_to_keep: 20,
            dry_run: false,
            refuse_overlap: true,
            log_editor: String::new(),
        };

        let cleared = settings.cleared_of_folders();

        assert!(cleared.source.is_empty());
        assert!(cleared.destination.is_empty());
        assert_eq!(cleared.mode, TransferMode::Copy);
        assert_eq!(cleared.usable_window_size(), (1000.0, 700.0));
        assert_eq!(cleared.usable_window_position(), Some((50.0, 60.0)));
        assert!(cleared.maximized, "a reset does not unmaximize the window");
        assert!(cleared.recent_sources.is_empty());
        assert!(cleared.recent_destinations.is_empty());
        assert!(cleared.extra_sources.is_empty());
        assert!(
            cleared.refuse_overlap,
            "a reset leaves the safety setting alone"
        );
    }

    #[test]
    fn refusing_an_overlapping_destination_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");

        assert!(!Settings::default().refuse_overlap, "off unless asked for");

        let settings = Settings {
            refuse_overlap: true,
            ..Settings::default()
        };
        Settings::save_to(&path, &settings).unwrap();

        assert!(Settings::load_from(&path).unwrap().refuse_overlap);
        assert!(
            fs::read_to_string(&path).unwrap().contains("refuse_overlap=1"),
            "written only when it is on"
        );

        // Anything that is not a clear yes leaves it off.
        fs::write(&path, "refuse_overlap=maybe\n").unwrap();
        assert!(!Settings::load_from(&path).unwrap().refuse_overlap);
    }

    #[test]
    fn a_chosen_log_editor_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");

        assert_eq!(Settings::default().log_editor, "", "Notepad unless chosen");

        let settings = Settings {
            log_editor: r"C:\Tools\editor.exe".to_owned(),
            ..Settings::default()
        };
        Settings::save_to(&path, &settings).unwrap();

        assert_eq!(
            Settings::load_from(&path).unwrap().log_editor,
            r"C:\Tools\editor.exe"
        );
        assert!(
            fs::read_to_string(&path).unwrap().contains("log_editor="),
            "written only when one is chosen"
        );

        // Cleared settings keep the choice.
        assert_eq!(settings.cleared_of_folders().log_editor, r"C:\Tools\editor.exe");
    }

    #[test]
    fn extra_sources_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");
        let settings = Settings {
            source: r"D:\one".to_owned(),
            destination: r"E:\library".to_owned(),
            extra_sources: vec![r"D:\two".to_owned(), r"F:\card".to_owned()],
            ..Settings::default()
        };

        Settings::save_to(&path, &settings).unwrap();
        let loaded = Settings::load_from(&path).unwrap();

        assert_eq!(loaded.source, r"D:\one");
        assert_eq!(
            loaded.extra_sources,
            vec![r"D:\two".to_owned(), r"F:\card".to_owned()]
        );
        assert_eq!(loaded.destination, r"E:\library");
    }

    #[test]
    fn recent_folders_round_trip_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");
        let mut settings = Settings::default();
        Settings::remember(&mut settings.recent_sources, r"D:\one");
        Settings::remember(&mut settings.recent_sources, r"D:\two");
        Settings::remember(&mut settings.recent_destinations, r"E:\library");

        Settings::save_to(&path, &settings).unwrap();
        let loaded = Settings::load_from(&path).unwrap();

        assert_eq!(
            loaded.recent_sources,
            vec![r"D:\two".to_owned(), r"D:\one".to_owned()]
        );
        assert_eq!(loaded.recent_destinations, vec![r"E:\library".to_owned()]);
        assert_eq!(loaded.source, "");
    }

    #[test]
    fn remembering_a_folder_moves_it_to_the_front_once() {
        let mut history = vec![r"D:\a".to_owned(), r"D:\b".to_owned()];

        Settings::remember(&mut history, r"d:\B");
        assert_eq!(history, vec![r"d:\B".to_owned(), r"D:\a".to_owned()]);

        Settings::remember(&mut history, "   ");
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn recent_lists_stay_short() {
        let mut history = Vec::new();
        for index in 0..20 {
            Settings::remember(&mut history, &format!(r"D:\folder{index}"));
        }

        assert_eq!(history.len(), super::MAX_RECENT);
        assert_eq!(history[0], r"D:\folder19");
    }

    #[test]
    fn cleared_fields_stay_empty() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");

        Settings::save_to(&path, &Settings::default()).unwrap();
        let loaded = Settings::load_from(&path).unwrap();

        assert_eq!(loaded, Settings::default());
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
    }

    #[test]
    fn missing_file_loads_nothing() {
        let directory = tempfile::tempdir().unwrap();
        assert!(Settings::load_from(&directory.path().join("absent.txt")).is_none());
    }

    #[test]
    fn ignores_junk_lines() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");
        fs::write(&path, "junk\nsource=D:\\x\n\nno-equals-here\ndestination=E:\\y\n").unwrap();

        let loaded = Settings::load_from(&path).unwrap();

        assert_eq!(loaded.source, r"D:\x");
        assert_eq!(loaded.destination, r"E:\y");
    }

    #[test]
    fn empty_file_is_allowed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");
        fs::write(&path, "").unwrap();

        assert_eq!(Settings::load_from(&path).unwrap(), Settings::default());
        assert!(Path::new(&path).is_file());
    }
}
