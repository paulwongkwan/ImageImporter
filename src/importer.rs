//! The import engine: walk a source tree, work out when each photo was taken and
//! file it under `<destination>/<year>/<year-month-day>/`.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use walkdir::WalkDir;

use crate::metadata::{self, DateOrigin};

/// Extensions treated as importable images: common raster formats plus the RAW
/// formats of every major camera vendor.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    // Common raster formats
    "jpg", "jpeg", "jpe", "jfif", "png", "bmp", "dib", "gif", "tif", "tiff", "webp", "heic",
    "heif", "avif", "jxl", // Camera RAW
    "dng", "cr2", "cr3", "crw", "nef", "nrw", "arw", "sr2", "srf", "raf", "orf", "rw2", "raw",
    "pef", "srw", "x3f", "rwl", "3fr", "fff", "iiq", "mrw", "dcr", "kdc", "erf", "mef", "mos",
];

/// Files that describe edits to a photo and have to travel with it: Adobe
/// Lightroom/Camera Raw (`xmp`), iPhone adjustments (`aae`), RawTherapee (`pp3`)
/// and DxO (`dop`).
pub const SIDECAR_EXTENSIONS: &[&str] = &["xmp", "aae", "pp3", "dop"];

/// Extensions treated as importable video: the containers cameras, phones and
/// editors write.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mov", "mp4", "m4v", "3gp", "mts", "m2ts", "avi", "mkv", "wmv", "mpg", "mpeg", "ogv",
    "webm",
];

/// Whether `path` has an extension the importer recognises.
pub fn is_supported_image(path: &Path) -> bool {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) => {
            let extension = extension.to_ascii_lowercase();
            IMAGE_EXTENSIONS.contains(&extension.as_str())
        }
        None => false,
    }
}

/// Whether `path` is an edit sidecar rather than a picture.
pub fn is_sidecar(path: &Path) -> bool {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) => {
            let extension = extension.to_ascii_lowercase();
            SIDECAR_EXTENSIONS.contains(&extension.as_str())
        }
        None => false,
    }
}

/// Whether `path` is a video the importer takes.
pub fn is_supported_video(path: &Path) -> bool {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) => {
            let extension = extension.to_ascii_lowercase();
            VIDEO_EXTENSIONS.contains(&extension.as_str())
        }
        None => false,
    }
}

/// Whether `path` is anything the importer copies: a photo or a video.
pub fn is_importable(path: &Path) -> bool {
    is_supported_image(path) || is_supported_video(path)
}

/// A sidecar file and the place it belongs at.
#[derive(Clone, Debug)]
pub struct SidecarMove {
    pub from: PathBuf,
    pub to: PathBuf,
    /// Size of the sidecar in bytes, read before it travels.
    pub size: u64,
}

/// What to do with the files that were imported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TransferMode {
    /// Leave the originals in place.
    #[default]
    Copy,
    /// Remove the originals once they have been copied to the destination.
    Move,
}

/// A fully validated import job.
pub struct ImportRequest {
    /// Folders to import from; every sub-folder of each is included.
    pub sources: Vec<PathBuf>,
    pub destination: PathBuf,
    pub mode: TransferMode,
    /// Plan the import and report destinations without writing anything.
    pub dry_run: bool,
}

/// Progress notifications produced while the job runs.
pub enum Event {
    /// The source tree is being walked.
    Scanning,
    /// How many importable files were found.
    Discovered(usize),
    Imported {
        index: usize,
        total: usize,
        from: PathBuf,
        to: PathBuf,
        origin: DateOrigin,
        /// Size of the photo in bytes, read before it was moved.
        size: u64,
        /// The date the photo was filed under, as `YYYY-MM-DD HH:MM:SS`.
        taken: String,
        /// Sidecars that were carried over with this photo.
        sidecars: Vec<SidecarMove>,
    },
    /// A dry run: this file would be written to `to`, nothing was copied.
    Planned {
        index: usize,
        total: usize,
        from: PathBuf,
        to: PathBuf,
        origin: DateOrigin,
        size: u64,
        taken: String,
        sidecars: Vec<SidecarMove>,
    },
    /// The destination already holds an identical file, so nothing was copied.
    /// `sidecars` lists edits that were added to the library even so.
    Duplicate {
        index: usize,
        total: usize,
        from: PathBuf,
        to: PathBuf,
        /// Size of the source file in bytes.
        size: u64,
        /// The date the photo resolved to, as `YYYY-MM-DD HH:MM:SS`.
        taken: String,
        sidecars: Vec<SidecarMove>,
    },
    Failed {
        index: usize,
        total: usize,
        from: PathBuf,
        /// Size of the file in bytes, as far as it could be read.
        size: u64,
        /// The date the file resolved to, as `YYYY-MM-DD HH:MM:SS`.
        taken: String,
        reason: String,
    },
    /// A Move copied the file but could not delete the original, so the copy is
    /// in the destination and the original is still in the source folder.
    LeftBehind {
        from: PathBuf,
        to: PathBuf,
        reason: String,
    },
    /// A source folder left alone because it sits inside the destination, where
    /// everything already is the library or the app's own output.
    SkippedSource { folder: PathBuf },
    Finished(Summary),
    /// The user pressed Cancel.
    Aborted(Summary),
    /// The job could not even start.
    Fatal(String),
}

/// Totals for a finished (or aborted) job.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub imported: usize,
    pub duplicates: usize,
    pub failed: usize,
    /// Sidecars copied alongside their photos.
    pub sidecars: usize,
    /// Bytes written: the photos imported plus their sidecars.
    pub bytes: u64,
    /// Files imported from each source folder, in the order they were listed.
    pub per_source: Vec<usize>,
    /// Files already in the library, per source folder, in the same order.
    pub per_source_duplicates: Vec<usize>,
    /// Files that failed, per source folder, in the same order.
    pub per_source_failed: Vec<usize>,
    /// Photos with no sidecar in their own folder whose sidecar appears
    /// elsewhere in the scanned tree, paired as `(photo, sidecar)`.
    pub stray_sidecars: Vec<(PathBuf, PathBuf)>,
    pub total: usize,
    pub elapsed: Duration,
}

/// Run one import job to completion.
///
/// `cancel` is polled between files so a long batch can be stopped from the UI.
///
/// With [`ImportRequest::dry_run`] set, no folder or file is created: planned
/// destinations are reported through [`Event::Planned`] and counted in
/// [`Summary::imported`], while duplicates and failures are reported as usual.
pub fn run(
    request: &ImportRequest,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(Event),
) -> Result<Summary, String> {
    let started = Instant::now();

    if request.sources.is_empty() {
        return Err("Pick at least one source folder.".to_owned());
    }
    if let Some(missing) = request.sources.iter().find(|folder| !folder.is_dir()) {
        return Err(format!(
            "Source folder does not exist: {}",
            missing.display()
        ));
    }

    let mut source_roots = Vec::with_capacity(request.sources.len());
    for folder in &request.sources {
        source_roots.push(fs::canonicalize(folder).map_err(|error| {
            format!("Cannot open source folder {}: {error}", folder.display())
        })?);
    }

    // A dry run writes nothing, so a destination that does not exist yet is
    // described by its absolute path instead of being created.
    let destination_root = if request.dry_run {
        fs::canonicalize(&request.destination)
            .unwrap_or_else(|_| absolute(&request.destination))
    } else {
        fs::create_dir_all(&request.destination).map_err(|error| {
            format!(
                "Cannot create destination folder {}: {error}",
                request.destination.display()
            )
        })?;
        fs::canonicalize(&request.destination).map_err(|error| {
            format!(
                "Cannot open destination folder {}: {error}",
                request.destination.display()
            )
        })?
    };

    emit(Event::Scanning);

    // Recursive walk of every sub-folder. Images are the work; sidecars are
    // indexed so each photo can take its edits along. The destination is
    // skipped so importing into a folder below the source cannot feed on itself.
    let mut walks: Vec<Vec<PathBuf>> = Vec::new();
    let mut sidecars: HashMap<(PathBuf, String), PathBuf> = HashMap::new();

    // Sources are walked in the order they were listed, and each source's own
    // files are sorted so a run stays repeatable. That way the folder at the top
    // of the list is genuinely the one imported first.
    for (listed, root) in source_roots.iter().enumerate() {
        let mut found: Vec<PathBuf> = Vec::new();

        // A source that is the destination, or lies inside it, is the library
        // itself or a folder already filed under it. Walking it would re-import
        // the app's own output, so it is skipped and the run says which one.
        if is_inside(root, &destination_root) {
            emit(Event::SkippedSource {
                folder: request.sources[listed].clone(),
            });
            walks.push(found);
            continue;
        }

        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| entry.path() != destination_root)
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.into_path();
            if is_importable(&path) {
                found.push(path);
            } else if is_sidecar(&path) {
                // Keyed by folder as well as name: two cards in one tree routinely
                // hold different photos called `IMG_0001.CR3`.
                if let (Some(parent), Some(name)) =
                    (path.parent(), path.file_name().and_then(|name| name.to_str()))
                {
                    sidecars.insert((parent.to_path_buf(), name.to_ascii_lowercase()), path);
                }
            }
        }

        found.sort();
        walks.push(found);
    }

    // A folder queued inside another source gets walked twice; keeping the first
    // occurrence drops the repeats without disturbing the order of the list. Each
    // kept file remembers which folder it came from, for the per-source summary.
    let mut files: Vec<PathBuf> = Vec::new();
    let mut file_sources: Vec<usize> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for (index, found) in walks.into_iter().enumerate() {
        for path in found {
            if seen.insert(path.clone()) {
                files.push(path);
                file_sources.push(index);
            }
        }
    }

    // The folder-scoped index is what matches sidecars to photos; this one is
    // keyed by name alone, so a photo with no sidecar beside it can still be
    // checked against edits that were filed somewhere else in the tree.
    let mut sidecars_by_name: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for path in sidecars.values() {
        if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
            sidecars_by_name
                .entry(name.to_ascii_lowercase())
                .or_default()
                .push(path.clone());
        }
    }
    for paths in sidecars_by_name.values_mut() {
        paths.sort();
    }

    let total = files.len();
    emit(Event::Discovered(total));

    let mut summary = Summary {
        total,
        per_source: vec![0; source_roots.len()],
        per_source_duplicates: vec![0; source_roots.len()],
        per_source_failed: vec![0; source_roots.len()],
        ..Summary::default()
    };

    for (offset, path) in files.iter().enumerate() {
        let index = offset + 1;
        let source_index = file_sources[offset];

        if cancel.load(Ordering::Relaxed) {
            summary.elapsed = started.elapsed();
            emit(Event::Aborted(summary.clone()));
            return Ok(summary);
        }

        let taken = metadata::shooting_time(path);
        let taken_label = taken.when.format("%Y-%m-%d %H:%M:%S").to_string();
        // Read the size now, while the source file still exists in Move mode.
        let size = fs::metadata(path).map(|metadata| metadata.len()).unwrap_or(0);
        let directory = destination_root
            .join(taken.when.format("%Y").to_string())
            .join(taken.when.format("%Y-%m-%d").to_string());

        if !request.dry_run {
            if let Err(error) = fs::create_dir_all(&directory) {
                summary.failed += 1;
                summary.per_source_failed[source_index] += 1;
                emit(Event::Failed {
                    index,
                    total,
                    from: path.clone(),
                    size,
                    taken: taken_label,
                    reason: format!("cannot create {}: {error}", directory.display()),
                });
                continue;
            }
        }

        let (target, already_present) = match resolve_target(&directory, path) {
            Ok(Resolved::Fresh(target)) => (target, false),
            Ok(Resolved::AlreadyPresent(existing)) => (existing, true),
            Err(error) => {
                summary.failed += 1;
                summary.per_source_failed[source_index] += 1;
                emit(Event::Failed {
                    index,
                    total,
                    from: path.clone(),
                    size,
                    taken: taken_label,
                    reason: error.to_string(),
                });
                continue;
            }
        };

        let planned_sidecars = sidecar_plan(path, &target, &sidecars);

        if planned_sidecars.is_empty() {
            if let Some(elsewhere) = stray_sidecar(path, &sidecars_by_name) {
                summary.stray_sidecars.push((path.clone(), elsewhere));
            }
        }

        if already_present {
            summary.duplicates += 1;
            summary.per_source_duplicates[source_index] += 1;

            // The photo is in the library already, but edits made since the last
            // import still belong there.
            let carried = if request.dry_run {
                let missing: Vec<SidecarMove> = planned_sidecars
                    .into_iter()
                    .filter(|sidecar| !sidecar.to.exists())
                    .collect();
                summary.sidecars += missing.len();
                summary.bytes += total_sidecar_bytes(&missing);
                missing
            } else {
                transfer_sidecars(
                    &planned_sidecars,
                    request.mode,
                    &mut summary,
                    source_index,
                    index,
                    total,
                    &taken_label,
                    emit,
                )
            };

            emit(Event::Duplicate {
                index,
                total,
                from: path.clone(),
                to: directory,
                size,
                taken: taken_label,
                sidecars: carried,
            });
            continue;
        }

        if request.dry_run {
            summary.imported += 1;
            summary.per_source[source_index] += 1;
            summary.sidecars += planned_sidecars.len();
            summary.bytes += size + total_sidecar_bytes(&planned_sidecars);
            emit(Event::Planned {
                index,
                total,
                from: path.clone(),
                to: target,
                origin: taken.origin,
                size,
                taken: taken_label,
                sidecars: planned_sidecars,
            });
            continue;
        }

        match transfer(path, &target, request.mode) {
            Ok(outcome) => {
                summary.imported += 1;
                summary.per_source[source_index] += 1;
                summary.bytes += size;
                let carried =
                    transfer_sidecars(
                    &planned_sidecars,
                    request.mode,
                    &mut summary,
                    source_index,
                    index,
                    total,
                    &taken_label,
                    emit,
                );
                emit(Event::Imported {
                    index,
                    total,
                    from: path.clone(),
                    to: target.clone(),
                    origin: taken.origin,
                    size,
                    taken: taken_label,
                    sidecars: carried,
                });
                if let Transferred::CopiedNotRemoved(error) = outcome {
                    emit(Event::LeftBehind {
                        from: path.clone(),
                        to: target,
                        reason: error.to_string(),
                    });
                }
            }
            Err(error) => {
                summary.failed += 1;
                summary.per_source_failed[source_index] += 1;
                emit(Event::Failed {
                    index,
                    total,
                    from: path.clone(),
                    size,
                    taken: taken_label,
                    reason: error.to_string(),
                });
            }
        }
    }

    summary.elapsed = started.elapsed();
    emit(Event::Finished(summary.clone()));

    Ok(summary)
}

/// Whether `folder` is the destination itself or sits somewhere below it.
fn is_inside(folder: &Path, destination: &Path) -> bool {
    let folder = comparable(folder);
    let destination = comparable(destination);

    folder == destination || folder.starts_with(&format!("{destination}\\"))
}

/// A path made comparable with another: no verbatim prefix, one separator style,
/// no trailing separator, and lower case, because Windows folder names are not
/// case sensitive.
fn comparable(path: &Path) -> String {
    let mut text = path.to_string_lossy().replace('/', "\\");
    if let Some(rest) = text.strip_prefix("\\\\?\\") {
        text = rest.to_owned();
    }

    text.trim_end_matches('\\').to_ascii_lowercase()
}

/// Total size of a set of sidecars, for the summary.
fn total_sidecar_bytes(sidecars: &[SidecarMove]) -> u64 {
    sidecars.iter().map(|sidecar| sidecar.size).sum()
}

/// Carry over the sidecars that are not in the destination yet.
fn transfer_sidecars(
    planned: &[SidecarMove],
    mode: TransferMode,
    summary: &mut Summary,
    // The source folder the photo came from, so a failed sidecar counts there too.
    source: usize,
    index: usize,
    total: usize,
    taken: &str,
    emit: &mut dyn FnMut(Event),
) -> Vec<SidecarMove> {
    let mut carried = Vec::new();

    for sidecar in planned {
        if sidecar.to.exists() {
            continue;
        }
        match transfer(&sidecar.from, &sidecar.to, mode) {
            Ok(outcome) => {
                summary.sidecars += 1;
                summary.bytes += sidecar.size;
                carried.push(sidecar.clone());
                if let Transferred::CopiedNotRemoved(error) = outcome {
                    emit(Event::LeftBehind {
                        from: sidecar.from.clone(),
                        to: sidecar.to.clone(),
                        reason: error.to_string(),
                    });
                }
            }
            Err(error) => {
                summary.failed += 1;
                summary.per_source_failed[source] += 1;
                emit(Event::Failed {
                    index,
                    total,
                    from: sidecar.from.clone(),
                    size: sidecar.size,
                    taken: taken.to_owned(),
                    reason: format!("sidecar: {error}"),
                });
            }
        }
    }

    carried
}

/// Find the sidecars belonging to `photo` and where they belong once it lands
/// at `target`.
///
/// Two naming conventions are in use: `photo.cr3.xmp` (Lightroom) and
/// `photo.xmp` / `photo.AAE` (Apple, RawTherapee, DxO). Renamed photos keep
/// their sidecars matched, because the target name is derived from the target
/// photo name with the sidecar's own extension text preserved.
fn sidecar_plan(
    photo: &Path,
    target: &Path,
    sidecars: &HashMap<(PathBuf, String), PathBuf>,
) -> Vec<SidecarMove> {
    let (Some(parent), Some(target_dir)) = (photo.parent(), target.parent()) else {
        return Vec::new();
    };
    let (Some(photo_name), Some(photo_stem)) = (
        photo.file_name().and_then(|name| name.to_str()),
        photo.file_stem().and_then(|name| name.to_str()),
    ) else {
        return Vec::new();
    };
    let Some(target_name) = target.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let target_stem = target
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or(target_name);

    let mut plan = Vec::new();

    for extension in SIDECAR_EXTENSIONS {
        for (prefix, target_prefix) in [(photo_name, target_name), (photo_stem, target_stem)] {
            let key = (
                parent.to_path_buf(),
                format!("{}.{extension}", prefix.to_ascii_lowercase()),
            );
            let Some(from) = sidecars.get(&key) else {
                continue;
            };
            let Some(source_name) = from.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let rest = source_name.get(prefix.len()..).unwrap_or("");

            plan.push(SidecarMove {
                from: from.clone(),
                to: target_dir.join(format!("{target_prefix}{rest}")),
                size: fs::metadata(from).map(|metadata| metadata.len()).unwrap_or(0),
            });
        }
    }

    plan
}

/// Find a sidecar for `photo` that sits in a *different* folder.
///
/// Sidecars are matched per folder, so one filed elsewhere is not copied - but
/// it usually means an edit got separated from its photo, which is worth
/// naming at the end of the run.
fn stray_sidecar(
    photo: &Path,
    sidecars_by_name: &HashMap<String, Vec<PathBuf>>,
) -> Option<PathBuf> {
    let (Some(parent), Some(name), Some(stem)) = (
        photo.parent(),
        photo.file_name().and_then(|name| name.to_str()),
        photo.file_stem().and_then(|name| name.to_str()),
    ) else {
        return None;
    };

    for extension in SIDECAR_EXTENSIONS {
        for prefix in [name, stem] {
            let key = format!("{}.{extension}", prefix.to_ascii_lowercase());
            let Some(paths) = sidecars_by_name.get(&key) else {
                continue;
            };
            for candidate in paths {
                if candidate.parent() != Some(parent) {
                    return Some(candidate.clone());
                }
            }
        }
    }

    None
}

/// Make `path` absolute without requiring it to exist.
fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|current| current.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// What the destination holds for one source file.
enum Resolved {
    /// Nothing is at this path yet, so the file can be written there.
    Fresh(PathBuf),
    /// An identical file is already there.
    AlreadyPresent(PathBuf),
}

/// Pick the destination file name for `source` inside `directory`.
fn resolve_target(directory: &Path, source: &Path) -> io::Result<Resolved> {
    let file_name = source
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source file has no name"))?;

    let first = directory.join(file_name);
    if !first.exists() {
        return Ok(Resolved::Fresh(first));
    }
    if same_contents(source, &first)? {
        return Ok(Resolved::AlreadyPresent(first));
    }

    // Same name, different picture: keep both, numbered like Explorer would.
    let stem = source
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = source
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();

    for suffix in 1..10_000 {
        let candidate = directory.join(format!("{stem}_{suffix}{extension}"));
        if !candidate.exists() {
            return Ok(Resolved::Fresh(candidate));
        }
        if same_contents(source, &candidate)? {
            return Ok(Resolved::AlreadyPresent(candidate));
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many files with the same name in the destination folder",
    ))
}

fn same_contents(left: &Path, right: &Path) -> io::Result<bool> {
    let left_size = fs::metadata(left)?.len();
    let right_size = fs::metadata(right)?.len();
    if left_size != right_size {
        return Ok(false);
    }
    if left_size == 0 {
        return Ok(true);
    }

    let mut left_file = File::open(left)?;
    let mut right_file = File::open(right)?;
    let mut left_buffer = [0u8; 64 * 1024];
    let mut right_buffer = [0u8; 64 * 1024];

    loop {
        let left_read = read_full(&mut left_file, &mut left_buffer)?;
        let right_read = read_full(&mut right_file, &mut right_buffer)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

/// Fill as much of `buffer` as the file can provide, tolerating short reads.
fn read_full(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// How a transfer ended, so a Move that copied but could not tidy up can say so
/// rather than look like a plain failure.
enum Transferred {
    Done,
    /// The copy is in the destination, but the original is still in the source.
    CopiedNotRemoved(io::Error),
}

fn transfer(source: &Path, target: &Path, mode: TransferMode) -> io::Result<Transferred> {
    match mode {
        TransferMode::Copy => {
            copy_preserving_times(source, target).map(|()| Transferred::Done)
        }
        TransferMode::Move => match fs::rename(source, target) {
            Ok(()) => Ok(Transferred::Done),
            // Moving across volumes (C: -> D:) cannot be renamed, so copy and delete.
            Err(_) => {
                copy_preserving_times(source, target)?;
                match fs::remove_file(source) {
                    Ok(()) => Ok(Transferred::Done),
                    // The file is in the library either way; only the original
                    // stayed behind, and the log should say exactly that.
                    Err(error) => Ok(Transferred::CopiedNotRemoved(error)),
                }
            }
        },
    }
}

fn copy_preserving_times(source: &Path, target: &Path) -> io::Result<()> {
    fs::copy(source, target)?;

    if let Ok(metadata) = fs::metadata(source) {
        let mut times = fs::FileTimes::new();
        if let Ok(modified) = metadata.modified() {
            times = times.set_modified(modified);
        }
        if let Ok(accessed) = metadata.accessed() {
            times = times.set_accessed(accessed);
        }
        if let Ok(file) = fs::OpenOptions::new().write(true).open(target) {
            let _ = file.set_times(times);
        }
    }

    Ok(())
}
