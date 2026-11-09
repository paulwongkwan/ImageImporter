//! End-to-end tests for the import engine, using generated image fixtures.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use image_importer::importer::{self, Event, ImportRequest, TransferMode};
use image_importer::metadata::{self, DateOrigin};

#[test]
fn a_webm_video_counts_as_importable() {
    assert!(importer::is_supported_video(Path::new("clip.webm")));
    assert!(importer::is_supported_video(Path::new("clip.WEBM")));
    assert!(importer::is_importable(Path::new("clip.webm")));
}

/// Build a minimal little-endian TIFF: IFD0 carries a `DateTime` tag and a
/// pointer to an Exif sub-IFD that carries `DateTimeOriginal`.
fn build_tiff(modified: &str, original: &str) -> Vec<u8> {
    const IFD0_OFFSET: u32 = 8;
    const IFD0_LEN: u32 = 2 + 12 * 2 + 4;
    const MODIFIED_OFFSET: u32 = IFD0_OFFSET + IFD0_LEN;
    const EXIF_IFD_OFFSET: u32 = MODIFIED_OFFSET + 20;
    const EXIF_IFD_LEN: u32 = 2 + 12 + 4;
    const ORIGINAL_OFFSET: u32 = EXIF_IFD_OFFSET + EXIF_IFD_LEN;

    let mut out = Vec::new();
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&IFD0_OFFSET.to_le_bytes());

    // IFD0: DateTime (306) and the Exif sub-IFD pointer (34665).
    out.extend_from_slice(&2u16.to_le_bytes());
    entry(&mut out, 306, 2, 20, MODIFIED_OFFSET);
    entry(&mut out, 34665, 4, 1, EXIF_IFD_OFFSET);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&ascii_value(modified));

    // Exif sub-IFD: DateTimeOriginal (36867).
    out.extend_from_slice(&1u16.to_le_bytes());
    entry(&mut out, 36867, 2, 20, ORIGINAL_OFFSET);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&ascii_value(original));

    out
}

fn entry(out: &mut Vec<u8>, tag: u16, field_type: u16, count: u32, value: u32) {
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&field_type.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
}

fn ascii_value(value: &str) -> [u8; 20] {
    assert!(value.len() <= 19, "EXIF timestamps are 19 characters");
    let mut bytes = [0u8; 20];
    bytes[..value.len()].copy_from_slice(value.as_bytes());
    bytes
}

/// Wrap a TIFF block in the APP1/Exif segment of a minimal JPEG.
fn build_jpeg(tiff: &[u8]) -> Vec<u8> {
    let segment_length = (2 + 6 + tiff.len()) as u16;
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
    out.extend_from_slice(&segment_length.to_be_bytes());
    out.extend_from_slice(b"Exif\0\0");
    out.extend_from_slice(tiff);
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

fn import(source: &Path, destination: &Path, mode: TransferMode) -> importer::Summary {
    let request = ImportRequest {
        sources: vec![source.to_path_buf()],
        destination: destination.to_path_buf(),
        mode,
        dry_run: false,
    };
    let cancel = AtomicBool::new(false);
    importer::run(&request, &cancel, &mut |_| {}).expect("import should run")
}

#[test]
fn a_source_inside_the_destination_is_skipped() {
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("library");
    let source = destination.join("incoming");
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("IMG_0001.jpg"),
        build_jpeg(&build_tiff("2024:05:06 07:08:09", "2024:05:06 07:08:09")),
    )
    .unwrap();

    let request = ImportRequest {
        sources: vec![source.clone()],
        destination: destination.clone(),
        mode: TransferMode::Copy,
        dry_run: false,
    };
    let cancel = AtomicBool::new(false);
    let mut skipped = Vec::new();
    let summary = importer::run(&request, &cancel, &mut |event| {
        if let Event::SkippedSource { folder } = event {
            skipped.push(folder);
        }
    })
    .expect("the run still completes");

    assert_eq!(summary.total, 0, "nothing is read from inside the library");
    assert_eq!(summary.imported, 0, "nothing is copied back into it");
    assert_eq!(skipped, vec![source.clone()], "the skipped folder is named");
    assert!(
        source.join("IMG_0001.jpg").is_file(),
        "the file is left where it was"
    );
}

/// Import from several source folders into one destination.
fn import_sources(
    sources: &[&Path],
    destination: &Path,
    mode: TransferMode,
) -> importer::Summary {
    let request = ImportRequest {
        sources: sources.iter().map(|path| path.to_path_buf()).collect(),
        destination: destination.to_path_buf(),
        mode,
        dry_run: false,
    };
    let cancel = AtomicBool::new(false);
    importer::run(&request, &cancel, &mut |_| {}).expect("import should run")
}

/// Run a dry run and collect the planned destinations of photos and sidecars.
fn plan(
    source: &Path,
    destination: &Path,
    mode: TransferMode,
) -> (importer::Summary, Vec<PathBuf>, Vec<PathBuf>) {
    let request = ImportRequest {
        sources: vec![source.to_path_buf()],
        destination: destination.to_path_buf(),
        mode,
        dry_run: true,
    };
    let cancel = AtomicBool::new(false);
    let mut planned = Vec::new();
    let mut sidecars = Vec::new();

    let summary = {
        let mut record = |event: Event| {
            if let Event::Planned {
                to,
                sidecars: planned_sidecars,
                ..
            } = event
            {
                planned.push(to);
                for sidecar in planned_sidecars {
                    sidecars.push(sidecar.to);
                }
            }
        };
        importer::run(&request, &cancel, &mut record).expect("dry run should run")
    };

    (summary, planned, sidecars)
}

/// Build a minimal QuickTime/MP4 file: a brand box plus `moov`/`mvhd`, which is
/// where .mov and .mp4 videos keep their recording time.
fn build_mov(creation_seconds: u32) -> Vec<u8> {
    let mut mvhd = Vec::new();
    mvhd.extend_from_slice(&108u32.to_be_bytes());
    mvhd.extend_from_slice(b"mvhd");
    mvhd.extend_from_slice(&[0, 0, 0, 0]); // version 0, no flags
    mvhd.extend_from_slice(&creation_seconds.to_be_bytes());
    mvhd.extend_from_slice(&creation_seconds.to_be_bytes()); // modification time
    mvhd.extend_from_slice(&600u32.to_be_bytes()); // timescale
    mvhd.extend_from_slice(&0u32.to_be_bytes()); // duration
    mvhd.extend_from_slice(&[0u8; 80]); // rate, volume, matrix, track count, ...
    assert_eq!(mvhd.len(), 108, "a version 0 mvhd box");

    let mut moov = Vec::new();
    moov.extend_from_slice(&((8 + mvhd.len()) as u32).to_be_bytes());
    moov.extend_from_slice(b"moov");
    moov.extend_from_slice(&mvhd);

    let mut out = Vec::new();
    out.extend_from_slice(&16u32.to_be_bytes());
    out.extend_from_slice(b"ftyp");
    out.extend_from_slice(b"qt  ");
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&moov);
    out
}

#[test]
fn a_photo_without_exif_is_dated_from_its_xmp_sidecar() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("in");
    let destination = directory.path().join("out");
    fs::create_dir_all(&source).unwrap();

    // A BMP carries no date the importer can read; the sidecar has to supply it.
    fs::write(source.join("scan.bmp"), b"BM").unwrap();
    fs::write(
        source.join("scan.xmp"),
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
             <rdf:Description xmp:CreateDate="2024-03-05T09:41:12"/>
           </x:xmpmeta>"#,
    )
    .unwrap();

    let taken = metadata::shooting_time(&source.join("scan.bmp"));
    assert_eq!(taken.origin, DateOrigin::XmpSidecar);

    let summary = import(&source, &destination, TransferMode::Copy);
    assert_eq!(summary.imported, 1);

    let day = destination.join("2024").join("2024-03-05");
    assert!(day.join("scan.bmp").is_file());
    // The sidecar still travels with its photo.
    assert!(day.join("scan.xmp").is_file());
}

#[test]
fn a_sidecar_without_a_date_leaves_the_file_time_in_charge() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("in");
    fs::create_dir_all(&source).unwrap();

    fs::write(source.join("scan.bmp"), b"BM").unwrap();
    fs::write(
        source.join("scan.xmp"),
        r#"<rdf:Description xmp:Rating="5"/>"#,
    )
    .unwrap();

    let taken = metadata::shooting_time(&source.join("scan.bmp"));
    assert!(
        matches!(
            taken.origin,
            DateOrigin::FileCreated | DateOrigin::FileModified
        ),
        "{:?}",
        taken.origin
    );
}

#[test]
fn dates_videos_from_their_movie_header() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("in");
    let destination = directory.path().join("out");
    fs::create_dir_all(&source).unwrap();

    // 2026-06-01 10:00:00 UTC, counted from the QuickTime epoch.
    let created = 2_082_844_800u32 + 1_780_308_000;
    fs::write(source.join("clip.mp4"), build_mov(created)).unwrap();
    fs::write(source.join("note.txt"), b"not a video").unwrap();

    let taken = metadata::shooting_time(&source.join("clip.mp4"));
    assert_eq!(taken.origin, DateOrigin::VideoCreated);

    let summary = import(&source, &destination, TransferMode::Copy);
    assert_eq!(summary.imported, 1, "the .txt is left alone");

    let year = taken.when.format("%Y").to_string();
    let day = taken.when.format("%Y-%m-%d").to_string();
    let landed = destination.join(year).join(day).join("clip.mp4");
    assert!(landed.is_file(), "{}", landed.display());
}

#[test]
fn dates_videos_without_a_header_from_the_file_time() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("in");
    let destination = directory.path().join("out");
    fs::create_dir_all(&source).unwrap();

    // Not a QuickTime file at all, so only the file time is left to go on.
    fs::write(source.join("broken.mov"), vec![0u8; 64]).unwrap();

    let summary = import(&source, &destination, TransferMode::Copy);
    assert_eq!(summary.imported, 1);

    let landed = walkdir::WalkDir::new(&destination)
        .into_iter()
        .flatten()
        .find(|entry| entry.file_name() == "broken.mov")
        .expect("the video is filed somewhere");
    assert!(landed.path().is_file());
}

#[test]
fn reads_shooting_date_from_tiff() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("shot.tif");
    fs::write(&path, build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33")).unwrap();

    let taken = metadata::shooting_time(&path);

    assert_eq!(taken.origin, DateOrigin::ExifOriginal);
    assert_eq!(taken.when.to_string(), "2026-06-01 10:22:33");
}

#[test]
fn reads_shooting_date_from_jpeg_app1() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("shot.jpg");
    let tiff = build_tiff("2025:01:02 03:04:05", "2024:12:24 18:30:00");
    fs::write(&path, build_jpeg(&tiff)).unwrap();

    let taken = metadata::shooting_time(&path);

    assert_eq!(taken.origin, DateOrigin::ExifOriginal);
    assert_eq!(taken.when.to_string(), "2024-12-24 18:30:00");
}

#[test]
fn falls_back_to_file_date_without_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("plain.bmp");
    fs::write(&path, vec![0u8; 128]).unwrap();

    let taken = metadata::shooting_time(&path);

    assert!(
        matches!(
            taken.origin,
            DateOrigin::FileCreated | DateOrigin::FileModified
        ),
        "unexpected origin: {:?}",
        taken.origin
    );
}

#[test]
fn files_land_in_year_and_day_folders() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let library = destination.path().join("library");

    fs::create_dir_all(source.path().join("trip/day one")).unwrap();
    fs::write(
        source.path().join("trip/day one/beach.jpg"),
        build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33")),
    )
    .unwrap();
    fs::write(source.path().join("trip/plain.bmp"), vec![0u8; 64]).unwrap();
    fs::write(source.path().join("trip/notes.txt"), b"not a photo").unwrap();

    let summary = import(source.path(), &library, TransferMode::Copy);

    assert_eq!(summary.imported, 2, "text files must be ignored");
    assert_eq!(summary.failed, 0);
    assert!(library.join("2026/2026-06-01/beach.jpg").is_file());

    let today = chrono::Local::now().date_naive();
    let undated = library
        .join(today.format("%Y").to_string())
        .join(today.format("%Y-%m-%d").to_string())
        .join("plain.bmp");
    assert!(undated.is_file(), "missing {}", undated.display());
}

#[test]
fn duplicate_events_carry_size_and_resolved_date() {
    let source = tempfile::tempdir().unwrap();
    // The destination deliberately sits inside the source tree, as above.
    let library = source.path().join("library");
    fs::write(
        source.path().join("a.jpg"),
        build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33")),
    )
    .unwrap();
    import(source.path(), &library, TransferMode::Copy);

    let request = ImportRequest {
        sources: vec![source.path().to_path_buf()],
        destination: library.clone(),
        mode: TransferMode::Copy,
        dry_run: false,
    };
    let cancel = AtomicBool::new(false);
    let mut reported = None;
    {
        let mut record = |event: Event| {
            if let Event::Duplicate {
                size, taken, ..
            } = event
            {
                reported = Some((size, taken));
            }
        };
        importer::run(&request, &cancel, &mut record).expect("second run should work");
    }

    let (size, taken) = reported.expect("a duplicate event");
    assert!(size > 0, "the skipped file still has a size");
    assert_eq!(taken, "2026-06-01 10:22:33");
}

#[test]
fn rerunning_the_same_import_skips_duplicates() {
    let source = tempfile::tempdir().unwrap();
    // The destination deliberately sits inside the source tree.
    let library = source.path().join("library");
    fs::write(
        source.path().join("a.jpg"),
        build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33")),
    )
    .unwrap();

    let first = import(source.path(), &library, TransferMode::Copy);
    assert_eq!(first.imported, 1);

    let second = import(source.path(), &library, TransferMode::Copy);
    assert_eq!(second.imported, 0);
    assert_eq!(second.duplicates, 1);
    assert_eq!(second.failed, 0);
    assert!(library.join("2026/2026-06-01/a.jpg").is_file());
}

#[test]
fn different_photos_with_the_same_name_both_survive() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let library = destination.path().join("library");

    fs::create_dir_all(source.path().join("one")).unwrap();
    fs::create_dir_all(source.path().join("two")).unwrap();
    fs::write(
        source.path().join("one/img.jpg"),
        build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:00:00")),
    )
    .unwrap();
    fs::write(
        source.path().join("two/img.jpg"),
        build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 11:00:00")),
    )
    .unwrap();

    let summary = import(source.path(), &library, TransferMode::Copy);

    assert_eq!(summary.imported, 2);
    assert!(library.join("2026/2026-06-01/img.jpg").is_file());
    assert!(library.join("2026/2026-06-01/img_1.jpg").is_file());
}

#[test]
fn move_removes_the_original() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let library = destination.path().join("library");
    let original = source.path().join("a.jpg");
    fs::write(
        &original,
        build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33")),
    )
    .unwrap();

    let summary = import(source.path(), &library, TransferMode::Move);

    assert_eq!(summary.imported, 1);
    assert!(!original.exists(), "the original should have been removed");
    assert!(library.join("2026/2026-06-01/a.jpg").is_file());
}

#[test]
fn missing_source_folder_is_reported() {
    let destination = tempfile::tempdir().unwrap();
    let request = ImportRequest {
        sources: vec![destination.path().join("does-not-exist")],
        destination: destination.path().join("library"),
        mode: TransferMode::Copy,
        dry_run: false,
    };
    let cancel = AtomicBool::new(false);

    let error = importer::run(&request, &cancel, &mut |_| {}).unwrap_err();

    assert!(error.contains("Source folder does not exist"), "{error}");
}

#[test]
fn dry_run_lists_destinations_and_creates_nothing() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let library = destination.path().join("library");

    fs::create_dir_all(source.path().join("trip")).unwrap();
    fs::write(
        source.path().join("trip/beach.jpg"),
        build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33")),
    )
    .unwrap();

    let (summary, planned, _sidecars) = plan(source.path(), &library, TransferMode::Copy);

    assert_eq!(summary.imported, 1);
    assert_eq!(summary.failed, 0);
    assert_eq!(planned.len(), 1);
    assert!(
        planned[0].ends_with(PathBuf::from("2026").join("2026-06-01").join("beach.jpg")),
        "unexpected plan: {}",
        planned[0].display()
    );
    assert!(
        !library.exists(),
        "a dry run must not create the destination"
    );
    assert!(source.path().join("trip/beach.jpg").is_file());
}

#[test]
fn dry_run_move_leaves_every_file_in_place() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let library = destination.path().join("library");
    let original = source.path().join("a.jpg");
    fs::write(
        &original,
        build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33")),
    )
    .unwrap();

    let (summary, planned, _sidecars) = plan(source.path(), &library, TransferMode::Move);

    assert_eq!(summary.imported, 1);
    assert_eq!(planned.len(), 1);
    assert!(original.is_file(), "a dry run must not move anything");
    assert!(!library.exists());
}

#[test]
fn dry_run_predicts_the_path_the_real_run_creates() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let library = destination.path().join("library");
    fs::write(
        source.path().join("a.jpg"),
        build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33")),
    )
    .unwrap();

    let (before, planned, _sidecars) = plan(source.path(), &library, TransferMode::Copy);
    assert_eq!(before.imported, 1);
    assert_eq!(planned.len(), 1);

    let real = import(source.path(), &library, TransferMode::Copy);
    assert_eq!(real.imported, 1);
    assert!(
        planned[0].is_file(),
        "the preview should name the file the real run created: {}",
        planned[0].display()
    );

    let (after, planned_after, _sidecars) = plan(source.path(), &library, TransferMode::Copy);
    assert_eq!(after.imported, 0);
    assert_eq!(after.duplicates, 1);
    assert!(
        planned_after.is_empty(),
        "an already imported file must show as a duplicate in the preview"
    );
}

#[test]
fn sidecars_follow_their_photo() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let library = destination.path().join("library");

    fs::write(
        source.path().join("shot.dng"),
        build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33"),
    )
    .unwrap();
    fs::write(source.path().join("shot.dng.xmp"), b"lightroom").unwrap();
    fs::write(source.path().join("shot.AAE"), b"iphone").unwrap();

    let summary = import(source.path(), &library, TransferMode::Copy);

    assert_eq!(summary.imported, 1);
    assert_eq!(summary.sidecars, 2);
    let day = library.join("2026/2026-06-01");
    assert!(day.join("shot.dng").is_file());
    assert_eq!(fs::read(day.join("shot.dng.xmp")).unwrap(), b"lightroom");
    assert_eq!(fs::read(day.join("shot.AAE")).unwrap(), b"iphone");
}

#[test]
fn sidecars_follow_a_renamed_photo() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let library = destination.path().join("library");

    fs::create_dir_all(source.path().join("one")).unwrap();
    fs::create_dir_all(source.path().join("two")).unwrap();
    fs::write(
        source.path().join("one/img.dng"),
        build_tiff("2025:01:02 03:04:05", "2026:06:01 10:00:00"),
    )
    .unwrap();
    fs::write(source.path().join("one/img.xmp"), b"one").unwrap();
    fs::write(
        source.path().join("two/img.dng"),
        build_tiff("2025:01:02 03:04:05", "2026:06:01 11:00:00"),
    )
    .unwrap();
    fs::write(source.path().join("two/img.xmp"), b"two").unwrap();

    let summary = import(source.path(), &library, TransferMode::Copy);

    assert_eq!(summary.imported, 2);
    assert_eq!(summary.sidecars, 2);
    let day = library.join("2026/2026-06-01");
    assert_eq!(fs::read(day.join("img.xmp")).unwrap(), b"one");
    assert_eq!(fs::read(day.join("img_1.xmp")).unwrap(), b"two");
}

#[test]
fn dry_run_lists_sidecars_without_copying_them() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let library = destination.path().join("library");

    fs::write(
        source.path().join("shot.dng"),
        build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33"),
    )
    .unwrap();
    fs::write(source.path().join("shot.dng.xmp"), b"lightroom").unwrap();

    let (summary, planned, sidecars) = plan(source.path(), &library, TransferMode::Copy);

    assert_eq!(planned.len(), 1);
    assert_eq!(sidecars.len(), 1);
    assert_eq!(summary.sidecars, 1);
    assert!(
        sidecars[0].ends_with(PathBuf::from("2026").join("2026-06-01").join("shot.dng.xmp")),
        "unexpected sidecar plan: {}",
        sidecars[0].display()
    );
    assert!(!library.exists(), "a dry run must not create the destination");
}

#[test]
fn move_mode_takes_sidecars_along() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let library = destination.path().join("library");

    fs::write(
        source.path().join("shot.dng"),
        build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33"),
    )
    .unwrap();
    fs::write(source.path().join("shot.dng.xmp"), b"lightroom").unwrap();

    let summary = import(source.path(), &library, TransferMode::Move);

    assert_eq!(summary.imported, 1);
    assert_eq!(summary.sidecars, 1);
    assert!(!source.path().join("shot.dng.xmp").exists());
    assert!(library.join("2026/2026-06-01/shot.dng.xmp").is_file());
}

#[test]
fn the_summary_totals_the_bytes_imported() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();

    let photo = build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33"));
    fs::write(source.path().join("IMG_0200.JPG"), &photo).unwrap();
    let edit = b"<x:xmpmeta>tone curve</x:xmpmeta>";
    fs::write(source.path().join("IMG_0200.xmp"), edit).unwrap();

    let request = ImportRequest {
        sources: vec![source.path().to_path_buf()],
        destination: destination.path().join("library"),
        mode: TransferMode::Copy,
        dry_run: false,
    };
    let cancel = AtomicBool::new(false);

    let summary = importer::run(&request, &cancel, &mut |_| {}).expect("import should run");

    assert_eq!(summary.imported, 1);
    assert_eq!(summary.sidecars, 1);
    assert_eq!(
        summary.bytes,
        photo.len() as u64 + edit.len() as u64,
        "the photo and its sidecar both count"
    );
}

#[test]
fn sidecar_events_report_their_size() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();

    fs::write(
        source.path().join("IMG_0100.JPG"),
        build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33")),
    )
    .unwrap();
    let edit = b"<x:xmpmeta>crop and exposure</x:xmpmeta>";
    fs::write(source.path().join("IMG_0100.xmp"), edit).unwrap();

    let request = ImportRequest {
        sources: vec![source.path().to_path_buf()],
        destination: destination.path().join("library"),
        mode: TransferMode::Copy,
        dry_run: false,
    };
    let cancel = AtomicBool::new(false);

    let mut sizes = Vec::new();
    importer::run(&request, &cancel, &mut |event| {
        if let Event::Imported { sidecars, .. } = event {
            sizes.extend(sidecars.iter().map(|sidecar| sidecar.size));
        }
    })
    .expect("import should run");

    assert_eq!(sizes, vec![edit.len() as u64]);
}

#[test]
fn a_sidecar_filed_in_another_folder_is_reported_at_the_end() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let library = destination.path().join("library");

    let card = source.path().join("card");
    let edits = source.path().join("edits");
    fs::create_dir_all(&card).unwrap();
    fs::create_dir_all(&edits).unwrap();

    fs::write(
        card.join("IMG_0231.JPG"),
        build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33")),
    )
    .unwrap();
    fs::write(edits.join("IMG_0231.JPG.xmp"), "<xmp/>").unwrap();

    let request = ImportRequest {
        sources: vec![source.path().to_path_buf()],
        destination: library.clone(),
        mode: TransferMode::Copy,
        dry_run: false,
    };
    let cancel = AtomicBool::new(false);

    let summary = importer::run(&request, &cancel, &mut |_| {}).expect("import should run");

    assert_eq!(summary.stray_sidecars.len(), 1);
    let (photo, sidecar) = &summary.stray_sidecars[0];
    // Paths come back canonical, so compare the leaf and the folder holding it.
    let leaf = |path: &std::path::Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    };
    let holder = |path: &std::path::Path| {
        path.parent()
            .and_then(|dir| dir.file_name())
            .map(|name| name.to_string_lossy().into_owned())
    };

    assert_eq!(holder(photo).as_deref(), Some("card"));
    assert_eq!(leaf(photo).as_deref(), Some("IMG_0231.JPG"));
    assert_eq!(holder(sidecar).as_deref(), Some("edits"));
    assert_eq!(leaf(sidecar).as_deref(), Some("IMG_0231.JPG.xmp"));
    assert!(
        !library.join("2026-06-01").join("IMG_0231.JPG.xmp").exists(),
        "a sidecar from another folder must not be copied"
    );
}

#[test]
fn a_sidecar_beside_its_photo_is_not_reported_as_stray() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();

    fs::write(
        source.path().join("IMG_0002.JPG"),
        build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33")),
    )
    .unwrap();
    fs::write(source.path().join("IMG_0002.xmp"), "<xmp/>").unwrap();

    let request = ImportRequest {
        sources: vec![source.path().to_path_buf()],
        destination: destination.path().join("library"),
        mode: TransferMode::Copy,
        dry_run: false,
    };
    let cancel = AtomicBool::new(false);

    let summary = importer::run(&request, &cancel, &mut |_| {}).expect("import should run");

    assert!(summary.stray_sidecars.is_empty(), "{summary:?}");
    assert_eq!(summary.sidecars, 1);
}

#[test]
fn sidecars_added_later_are_copied_for_an_already_imported_photo() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let library = destination.path().join("library");

    fs::write(
        source.path().join("shot.dng"),
        build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33"),
    )
    .unwrap();

    let first = import(source.path(), &library, TransferMode::Copy);
    assert_eq!(first.imported, 1);
    assert_eq!(first.sidecars, 0);

    // The edit is written after the photo was already imported.
    fs::write(source.path().join("shot.dng.xmp"), b"lightroom").unwrap();

    let second = import(source.path(), &library, TransferMode::Copy);
    assert_eq!(second.imported, 0);
    assert_eq!(second.duplicates, 1);
    assert_eq!(second.sidecars, 1);
    assert!(library.join("2026/2026-06-01/shot.dng.xmp").is_file());
}

#[test]
fn imported_events_carry_size_and_resolved_date() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let library = destination.path().join("library");

    let photo = build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33"));
    fs::write(source.path().join("shot.jpg"), &photo).unwrap();

    let request = ImportRequest {
        sources: vec![source.path().to_path_buf()],
        destination: library.clone(),
        mode: TransferMode::Copy,
        dry_run: false,
    };
    let cancel = AtomicBool::new(false);
    let mut reported = Vec::new();
    importer::run(&request, &cancel, &mut |event| {
        if let Event::Imported {
            to, size, taken, ..
        } = event
        {
            reported.push((to, size, taken));
        }
    })
    .expect("import should run");

    assert_eq!(reported.len(), 1);
    let (to, size, taken) = &reported[0];
    assert_eq!(*size, photo.len() as u64, "size of the source file");
    assert_eq!(taken, "2026-06-01 10:22:33", "the date it was filed under");
    assert!(to.ends_with("2026-06-01/shot.jpg"), "{} ", to.display());
}

#[test]
fn duplicates_and_failures_are_counted_per_source_folder() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();

    let photo = build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33"));
    fs::write(first.path().join("A.jpg"), &photo).unwrap();
    fs::write(second.path().join("B.jpg"), &photo).unwrap();

    let library = destination.path().join("library");
    let summary = import_sources(&[first.path(), second.path()], &library, TransferMode::Copy);
    assert_eq!(summary.imported, 2);
    assert_eq!(summary.per_source_failed, vec![0, 0]);

    let again = import_sources(&[first.path(), second.path()], &library, TransferMode::Copy);

    assert_eq!(again.imported, 0);
    assert_eq!(again.duplicates, 2);
    assert_eq!(again.per_source, vec![0, 0]);
    assert_eq!(again.per_source_duplicates, vec![1, 1]);
    assert_eq!(again.per_source_failed, vec![0, 0]);
}

#[test]
fn the_summary_counts_files_per_source_folder() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();

    let photo = build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33"));
    fs::write(first.path().join("A.jpg"), &photo).unwrap();
    fs::write(first.path().join("B.jpg"), &photo).unwrap();
    fs::write(second.path().join("C.jpg"), &photo).unwrap();

    let request = ImportRequest {
        sources: vec![first.path().to_path_buf(), second.path().to_path_buf()],
        destination: destination.path().join("library"),
        mode: TransferMode::Copy,
        dry_run: false,
    };
    let cancel = AtomicBool::new(false);

    let summary = importer::run(&request, &cancel, &mut |_| {}).expect("import should run");

    assert_eq!(summary.imported, 3);
    assert_eq!(
        summary.per_source,
        vec![2, 1],
        "the counts line up with the folders as they were listed"
    );
}

#[test]
fn sources_are_imported_in_the_order_they_are_listed() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();

    let photo = build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:22:33"));
    fs::write(first.path().join("A.jpg"), &photo).unwrap();
    fs::write(second.path().join("B.jpg"), &photo).unwrap();

    let request = ImportRequest {
        sources: vec![second.path().to_path_buf(), first.path().to_path_buf()],
        destination: destination.path().join("library"),
        mode: TransferMode::Copy,
        dry_run: false,
    };
    let cancel = AtomicBool::new(false);

    let mut order = Vec::new();
    importer::run(&request, &cancel, &mut |event| {
        if let Event::Imported { from, .. } = event {
            order.push(from.file_name().unwrap().to_string_lossy().into_owned());
        }
    })
    .expect("import should run");

    assert_eq!(
        order,
        ["B.jpg", "A.jpg"],
        "the folder listed first should be imported first"
    );
}

#[test]
fn several_sources_land_in_one_destination() {
    let card = tempfile::tempdir().unwrap();
    let backup = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let library = destination.path().join("library");

    fs::write(
        card.path().join("a.jpg"),
        build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:00:00")),
    )
    .unwrap();
    fs::write(
        backup.path().join("b.jpg"),
        build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:02 11:00:00")),
    )
    .unwrap();

    let summary = import_sources(&[card.path(), backup.path()], &library, TransferMode::Copy);

    assert_eq!(summary.total, 2);
    assert_eq!(summary.imported, 2);
    assert_eq!(summary.failed, 0);
    assert!(library.join("2026/2026-06-01/a.jpg").is_file());
    assert!(library.join("2026/2026-06-02/b.jpg").is_file());
}

#[test]
fn a_source_nested_inside_another_is_imported_once() {
    let source = tempfile::tempdir().unwrap();
    let inner = source.path().join("inner");
    fs::create_dir_all(&inner).unwrap();
    let destination = tempfile::tempdir().unwrap();
    let library = destination.path().join("library");

    fs::write(
        inner.join("p.jpg"),
        build_jpeg(&build_tiff("2025:01:02 03:04:05", "2026:06:01 10:00:00")),
    )
    .unwrap();

    let summary = import_sources(&[source.path(), &inner], &library, TransferMode::Copy);

    assert_eq!(summary.total, 1, "a nested source must not be walked twice");
    assert_eq!(summary.imported, 1);
    assert_eq!(summary.duplicates, 0);
}

#[test]
fn a_missing_source_stops_the_run_before_anything_is_copied() {
    let destination = tempfile::tempdir().unwrap();
    let library = destination.path().join("library");
    let request = ImportRequest {
        sources: vec![destination.path().join("gone")],
        destination: library.clone(),
        mode: TransferMode::Copy,
        dry_run: false,
    };
    let cancel = AtomicBool::new(false);

    let error = importer::run(&request, &cancel, &mut |_| {}).expect_err("should refuse");

    assert!(error.contains("does not exist"), "{error}");
    assert!(!library.exists());
}

#[test]
fn an_empty_source_list_is_refused() {
    let destination = tempfile::tempdir().unwrap();
    let request = ImportRequest {
        sources: Vec::new(),
        destination: destination.path().to_path_buf(),
        mode: TransferMode::Copy,
        dry_run: true,
    };
    let cancel = AtomicBool::new(false);

    let error = importer::run(&request, &cancel, &mut |_| {}).expect_err("should refuse");

    assert!(error.contains("at least one"), "{error}");
}
