//! Resolve the date a photo was taken.
//!
//! The EXIF `DateTimeOriginal` tag wins; `DateTimeDigitized` and the IFD0
//! `DateTime` tag are used when it is missing. Anything without usable metadata
//! falls back to the file's creation time (then modification time).

use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime};

/// How much of a file is read while hunting for an embedded EXIF block. Camera
/// RAW containers (CR3, RAF, ...) can hide the TIFF header far from the start,
/// so the tail of the file is searched as well before giving up.
const HEAD_SCAN_LIMIT: u64 = 4 * 1024 * 1024;
const TAIL_SCAN_LIMIT: u64 = 2 * 1024 * 1024;

/// Plausible range for a photo timestamp; anything outside is treated as junk.
const MIN_YEAR: i32 = 1900;
const MAX_YEAR: i32 = 2999;

/// Where a resolved timestamp came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateOrigin {
    /// EXIF `DateTimeOriginal` - the moment the shutter was released.
    ExifOriginal,
    /// EXIF `DateTimeDigitized`.
    ExifDigitized,
    /// EXIF `DateTime` stored in IFD0.
    ExifModified,
    /// The capture date written in an XMP sidecar.
    XmpSidecar,
    /// The `mvhd` creation time of an MP4/MOV video.
    VideoCreated,
    /// Windows file creation time.
    FileCreated,
    /// File modification time.
    FileModified,
    /// Last resort, when the file cannot be stat'ed at all.
    Clock,
}

impl DateOrigin {
    /// Short human readable description, shown in the log.
    pub fn describe(self) -> &'static str {
        match self {
            Self::ExifOriginal => "EXIF shooting date",
            Self::ExifDigitized => "EXIF digitised date",
            Self::ExifModified => "EXIF modified date",
            Self::XmpSidecar => "XMP sidecar date",
            Self::VideoCreated => "video creation date",
            Self::FileCreated => "file creation date",
            Self::FileModified => "file modified date",
            Self::Clock => "current time",
        }
    }
}

/// A resolved timestamp and its origin.
#[derive(Clone, Copy, Debug)]
pub struct Taken {
    pub when: NaiveDateTime,
    pub origin: DateOrigin,
}

/// Resolve the timestamp used to pick the destination folder for `path`.
pub fn shooting_time(path: &Path) -> Taken {
    if let Some(taken) = quicktime_time(path) {
        return taken;
    }

    if let Some(taken) = exif_time(path) {
        return taken;
    }

    // Edits and ratings live in the sidecar, and so can the capture date a
    // converted or stripped file lost.
    if let Some(taken) = sidecar_time(path) {
        return taken;
    }

    file_time(path)
}

fn exif_time(path: &Path) -> Option<Taken> {
    let exif = read_exif(path)?;

    let candidates = [
        (exif::Tag::DateTimeOriginal, DateOrigin::ExifOriginal),
        (exif::Tag::DateTimeDigitized, DateOrigin::ExifDigitized),
        (exif::Tag::DateTime, DateOrigin::ExifModified),
    ];

    for (tag, origin) in candidates {
        if let Some(raw) = ascii_field(&exif, tag) {
            if let Some(when) = parse_exif_datetime(&raw) {
                return Some(Taken { when, origin });
            }
        }
    }

    None
}

/// Read the EXIF block of an image, trying the container formats the `exif`
/// crate understands before searching for an embedded TIFF header.
fn read_exif(path: &Path) -> Option<exif::Exif> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(&file);
    if let Ok(exif) = exif::Reader::new().read_from_container(&mut reader) {
        return Some(exif);
    }

    let mut file = File::open(path).ok()?;
    embedded_exif(&mut file)
}

fn embedded_exif(file: &mut File) -> Option<exif::Exif> {
    let length = file.metadata().ok()?.len();

    let mut head = Vec::new();
    file.take(HEAD_SCAN_LIMIT).read_to_end(&mut head).ok()?;
    if let Some(offset) = find_tiff_header(&head) {
        return parse_tiff(&head[offset..]);
    }

    if length > HEAD_SCAN_LIMIT {
        let start = length.saturating_sub(TAIL_SCAN_LIMIT).max(HEAD_SCAN_LIMIT);
        let mut tail = Vec::new();
        file.seek(SeekFrom::Start(start)).ok()?;
        file.take(TAIL_SCAN_LIMIT).read_to_end(&mut tail).ok()?;
        if let Some(offset) = find_tiff_header(&tail) {
            return parse_tiff(&tail[offset..]);
        }
    }

    None
}

/// Locate a TIFF header (`II*\0` / `MM\0*`) inside a raw byte run.
fn find_tiff_header(data: &[u8]) -> Option<usize> {
    let mut index = 0;
    while index + 8 <= data.len() {
        let window = &data[index..index + 4];
        if window == b"II*\0" || window == b"MM\0*" {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn parse_tiff(data: &[u8]) -> Option<exif::Exif> {
    exif::Reader::new().read_raw(data.to_vec()).ok()
}

fn ascii_field(exif: &exif::Exif, tag: exif::Tag) -> Option<Vec<u8>> {
    let field = exif
        .get_field(tag, exif::In::PRIMARY)
        .or_else(|| exif.fields().find(|field| field.tag == tag))?;

    match &field.value {
        exif::Value::Ascii(chunks) => chunks.first().cloned(),
        _ => None,
    }
}

/// Parse an EXIF timestamp such as `2026:06:01 10:22:33`.
///
/// Values are local wall-clock times as written by the camera, so no time zone
/// conversion takes place. Broken placeholders (`0000:00:00 ...`) are rejected.
pub fn parse_exif_datetime(raw: &[u8]) -> Option<NaiveDateTime> {
    let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
    let text = std::str::from_utf8(&raw[..end]).ok()?.trim();
    if text.len() < 10 {
        return None;
    }

    // Normalise `YYYY-MM-DD 10:22:33` / `YYYY:MM:DD T10:22:33` to the canonical
    // EXIF spelling so a single parser handles the variations seen in the wild.
    let mut normalised = String::with_capacity(19);
    for (index, character) in text.chars().take(19).enumerate() {
        normalised.push(match index {
            4 | 7 => ':',
            10 => ' ',
            13 | 16 => ':',
            _ => character,
        });
    }
    if normalised.len() == 10 {
        normalised.push_str(" 00:00:00");
    }
    if normalised.len() != 19 {
        return None;
    }

    let parsed = NaiveDateTime::parse_from_str(&normalised, "%Y:%m:%d %H:%M:%S").ok()?;
    plausible(parsed)
}

fn plausible(when: NaiveDateTime) -> Option<NaiveDateTime> {
    (MIN_YEAR..=MAX_YEAR)
        .contains(&when.year())
        .then_some(when)
}

/// The recording time stored in the `mvhd` box of a QuickTime/MP4 file, which is
/// where .mov and .mp4 videos keep it. `None` for any other kind of file.
fn quicktime_time(path: &Path) -> Option<Taken> {
    let mut file = File::open(path).ok()?;

    let mut header = [0u8; 12];
    file.read_exact(&mut header).ok()?;
    // Every QuickTime/MP4 file begins with a brand box; anything else is not
    // worth walking box by box.
    if &header[4..8] != b"ftyp" {
        return None;
    }

    let length = file.metadata().ok()?.len();
    let mut offset = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as u64;

    while offset + 8 <= length {
        file.seek(SeekFrom::Start(offset)).ok()?;

        let mut box_header = [0u8; 16];
        file.read_exact(&mut box_header[..8]).ok()?;
        let mut size = u32::from_be_bytes([
            box_header[0],
            box_header[1],
            box_header[2],
            box_header[3],
        ]) as u64;
        let mut payload = offset + 8;

        if size == 1 {
            // A 64-bit `largesize` follows the usual 8 bytes of header.
            file.read_exact(&mut box_header[8..]).ok()?;
            size = u64::from_be_bytes(box_header[8..16].try_into().ok()?);
            payload = offset + 16;
        } else if size == 0 {
            // The box runs to the end of the file.
            size = length - offset;
        }

        if size < 8 {
            return None;
        }

        // `moov` holds the movie header; it sits at the start of files written
        // by phones and at the end of some camera files, so the walk continues
        // until it turns up.
        if &box_header[4..8] == b"moov" {
            return mvhd_time(&mut file, payload, offset + size);
        }

        offset += size;
    }

    None
}

/// Hunt the `mvhd` box inside a `moov` payload.
fn mvhd_time(file: &mut File, start: u64, end: u64) -> Option<Taken> {
    let mut offset = start;

    while offset + 8 <= end {
        file.seek(SeekFrom::Start(offset)).ok()?;

        let mut header = [0u8; 8];
        file.read_exact(&mut header).ok()?;
        let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as u64;

        if size < 8 {
            return None;
        }

        if &header[4..8] == b"mvhd" {
            // Only the version, flags and creation time are needed.
            let mut body = [0u8; 12];
            let wanted = ((size - 8) as usize).min(body.len());
            file.read_exact(&mut body[..wanted]).ok()?;

            return mvhd_created(&body[..wanted]).and_then(quicktime_taken);
        }

        offset += size;
    }

    None
}

/// The `creation_time` field of an `mvhd` body, whose width depends on the box
/// version: 32 bits in version 0, 64 in version 1.
fn mvhd_created(body: &[u8]) -> Option<u64> {
    match *body.first()? {
        0 => Some(u32::from_be_bytes(body.get(4..8)?.try_into().ok()?) as u64),
        1 => Some(u64::from_be_bytes(body.get(4..12)?.try_into().ok()?)),
        _ => None,
    }
}

/// Turn QuickTime seconds (counted from 1904-01-01 UTC) into the local wall clock
/// time used for destination folders.
fn quicktime_taken(seconds: u64) -> Option<Taken> {
    // 1904-01-01 to 1970-01-01, in seconds.
    const EPOCH_GAP: u64 = 2_082_844_800;

    let unix = seconds.checked_sub(EPOCH_GAP)?;
    let time = SystemTime::UNIX_EPOCH + Duration::from_secs(unix);
    let when = local_time(time).and_then(plausible)?;

    Some(Taken {
        when,
        origin: DateOrigin::VideoCreated,
    })
}

/// The capture date written in an XMP sidecar for `path`. Lightroom and friends
/// write `photo.xmp` beside the picture; some editors append instead
/// (`photo.RAW.xmp`), so both names are tried.
fn sidecar_time(path: &Path) -> Option<Taken> {
    let text = read_sidecar(path)?;

    // Camera-raw edits win over the general purpose fields.
    const TAGS: [&str; 4] = [
        "exif:DateTimeOriginal",
        "xmp:CreateDate",
        "photoshop:DateCreated",
        "tiff:DateTime",
    ];

    for tag in TAGS {
        if let Some(raw) = xmp_field(&text, tag) {
            if let Some(when) = parse_xmp_date(&raw) {
                return Some(Taken {
                    when,
                    origin: DateOrigin::XmpSidecar,
                });
            }
        }
    }

    None
}

/// Read the sidecar that belongs to `path`, whichever of the usual names it has.
fn read_sidecar(path: &Path) -> Option<String> {
    // XMP files are tiny; this only guards against a stray huge file.
    const LIMIT: u64 = 1024 * 1024;

    let mut appended = path.as_os_str().to_os_string();
    appended.push(".xmp");

    for candidate in [path.with_extension("xmp"), PathBuf::from(appended)] {
        if candidate == path {
            continue;
        }

        let Ok(file) = File::open(&candidate) else {
            continue;
        };

        let mut text = String::new();
        if file.take(LIMIT).read_to_string(&mut text).is_ok() {
            return Some(text);
        }
    }

    None
}

/// A value from an XMP document, written either as an element or as an attribute
/// of the `rdf:Description` tag.
fn xmp_field(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    if let Some(start) = text.find(&open) {
        let rest = &text[start + open.len()..];
        let end = rest.find('<')?;
        return Some(rest[..end].trim().to_owned());
    }

    let attribute = format!("{tag}=\"");
    let start = text.find(&attribute)?;
    let rest = &text[start + attribute.len()..];
    let end = rest.find('"')?;

    Some(rest[..end].trim().to_owned())
}

/// Parse the ISO 8601 timestamps XMP uses, ignoring any zone or fractional
/// second: the wall-clock time is what picks the folder.
fn parse_xmp_date(raw: &str) -> Option<NaiveDateTime> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }

    // `2026-06-01T10:22:33+08:00` and `2026-06-01T10:22:33.75Z` both stop at the
    // seconds, and a bare `2026-06-01` stops at the day.
    let stamp = &text[..text.len().min(19)];

    let parsed = NaiveDateTime::parse_from_str(stamp, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H:%M:%S"))
        .or_else(|_| NaiveDateTime::parse_from_str(stamp, "%Y-%m-%dT%H:%M"))
        .or_else(|_| {
            NaiveDate::parse_from_str(stamp, "%Y-%m-%d")
                .map(|date| date.and_hms_opt(0, 0, 0).expect("midnight exists"))
        })
        .ok()?;

    plausible(parsed)
}

fn file_time(path: &Path) -> Taken {
    if let Ok(metadata) = fs::metadata(path) {
        if let Some(when) = metadata
            .created()
            .ok()
            .and_then(local_time)
            .and_then(plausible)
        {
            return Taken {
                when,
                origin: DateOrigin::FileCreated,
            };
        }
        if let Some(when) = metadata
            .modified()
            .ok()
            .and_then(local_time)
            .and_then(plausible)
        {
            return Taken {
                when,
                origin: DateOrigin::FileModified,
            };
        }
    }

    Taken {
        when: Local::now().naive_local(),
        origin: DateOrigin::Clock,
    }
}

fn local_time(time: SystemTime) -> Option<NaiveDateTime> {
    let converted: DateTime<Local> = time.into();
    Some(converted.naive_local())
}

#[cfg(test)]
mod tests {
    use super::parse_exif_datetime;

    #[test]
    fn parses_canonical_exif_stamp() {
        let parsed = parse_exif_datetime(b"2026:06:01 10:22:33\0").unwrap();
        assert_eq!(parsed.to_string(), "2026-06-01 10:22:33");
    }

    #[test]
    fn parses_sub_second_and_dashed_variants() {
        assert_eq!(
            parse_exif_datetime(b"2026:06:01 10:22:33.75").unwrap().to_string(),
            "2026-06-01 10:22:33"
        );
        assert_eq!(
            parse_exif_datetime(b"2026-06-01T10:22:33").unwrap().to_string(),
            "2026-06-01 10:22:33"
        );
    }

    #[test]
    fn accepts_date_only_values() {
        assert_eq!(
            parse_exif_datetime(b"2026:06:01").unwrap().to_string(),
            "2026-06-01 00:00:00"
        );
    }

    #[test]
    fn xmp_fields_are_read_from_elements_and_attributes() {
        let attribute = r##"<rdf:Description xmp:CreateDate="2026-06-01T10:22:33"/>"##;
        assert_eq!(
            super::xmp_field(attribute, "xmp:CreateDate").as_deref(),
            Some("2026-06-01T10:22:33")
        );

        let element = "<exif:DateTimeOriginal>2026-06-02T08:00:00</exif:DateTimeOriginal>";
        assert_eq!(
            super::xmp_field(element, "exif:DateTimeOriginal").as_deref(),
            Some("2026-06-02T08:00:00")
        );

        assert!(super::xmp_field(element, "xmp:Rating").is_none());
    }

    #[test]
    fn xmp_dates_accept_zones_fractions_and_bare_days() {
        for raw in [
            "2026-06-01T10:22:33",
            "2026-06-01T10:22:33+08:00",
            "2026-06-01T10:22:33.75Z",
            "2026-06-01 10:22:33",
        ] {
            assert_eq!(
                super::parse_xmp_date(raw).expect(raw).to_string(),
                "2026-06-01 10:22:33",
                "{raw}"
            );
        }

        assert_eq!(
            super::parse_xmp_date("2026-06-01").expect("a bare day").to_string(),
            "2026-06-01 00:00:00"
        );
        assert!(super::parse_xmp_date("not a date").is_none());
        assert!(super::parse_xmp_date("").is_none());
    }

    #[test]
    fn mvhd_creation_time_is_read_from_both_box_versions() {
        let mut zero = vec![0u8; 12];
        zero[4..8].copy_from_slice(&2_082_844_800u32.to_be_bytes());
        assert_eq!(super::mvhd_created(&zero), Some(2_082_844_800));

        let mut one = vec![0u8; 12];
        one[0] = 1;
        one[4..12].copy_from_slice(&3_900_000_000u64.to_be_bytes());
        assert_eq!(super::mvhd_created(&one), Some(3_900_000_000));

        // Too short, and a version that does not exist, give nothing.
        assert_eq!(super::mvhd_created(&[]), None);
        let mut future = vec![0u8; 12];
        future[0] = 2;
        assert_eq!(super::mvhd_created(&future), None);
    }

    #[test]
    fn quicktime_zero_means_no_date() {
        assert!(super::quicktime_taken(0).is_none(), "a zeroed header");

        let taken = super::quicktime_taken(2_082_844_800 + 1_800_000_000).expect("a date");
        assert_eq!(taken.origin, super::DateOrigin::VideoCreated);
    }

    #[test]
    fn rejects_blank_and_placeholder_values() {
        assert!(parse_exif_datetime(b"0000:00:00 00:00:00").is_none());
        assert!(parse_exif_datetime(b"    :  :     :  :  ").is_none());
        assert!(parse_exif_datetime(b"").is_none());
        assert!(parse_exif_datetime(b"0000:00:00").is_none());
    }
}
