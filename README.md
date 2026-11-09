# Image Importer

A small Windows 11 desktop app, written in Rust, that batch-imports photos from a
source folder into a date-organised library.

Point it at a source folder (CD card, phone export, old backup) and a destination,
press **Start import**, and every image - including those buried in sub-folders -
is filed by the day it was taken:

```
<destination>/2026/2026-06-01/IMG_0231.JPG
<destination>/2026/2026-06-01/beach.CR3
<destination>/2026/2026-06-01/scan.DNG
```

## The interface

The window is built with [GPUI Kit](https://gpui-kit.com/), the component library
for Zed's GPUI framework: a native, GPU-drawn desktop interface with the standard
light and dark themes, real text fields with undo and selection, and keyboard
focus that moves through the window with Tab. The buttons and fields are the
kit's own components, so their hover, focus, disabled and pressed states come
from the theme rather than being redrawn by this app.

## Supported files

| Kind   | Extensions                                                                                                                  |
| ------ | --------------------------------------------------------------------------------------------------------------------------- |
| Raster | `jpg` `jpeg` `jpe` `jfif` `png` `bmp` `dib` `gif` `tif` `tiff` `webp` `heic` `heif` `avif` `jxl`                              |
| RAW    | `dng` `cr2` `cr3` `crw` `nef` `nrw` `arw` `sr2` `srf` `raf` `orf` `rw2` `raw` `pef` `srw` `x3f` `rwl` `3fr` `fff` `iiq` `mrw` `dcr` `kdc` `erf` `mef` `mos` |

| Video  | `mov` `mp4` `m4v` `3gp` `mts` `m2ts` `avi` `mkv` `wmv` `mpg` `mpeg` `ogv` `webm`                                                      |

Anything that is not a photo or a video is left alone, and files without a
supported extension are never copied. Videos are dated from the recording time
in their movie header, so a `.mov` or `.mp4` from a phone or camera lands in the
folder for the day it was shot; a video with no usable header falls back to the
file's creation time like any other file.

## Summary by day

When a run ends, the log - and therefore the run log file - closes with a count
per dated folder, oldest first:

```
Done: 55 imported, 12 duplicates skipped, 0 failed in 3.4s
Files per day folder:
  2026-06-01     43
  2026-06-02     12
```

Each line counts the photos filed into that folder during this run. Sidecars are
not counted per folder; they appear in the totals line instead. A dry run prints
the same table, headed `Files per day folder (planned):`, without copying
anything.

While a run is going the window shows those counts building up under the
progress bar as `Files per day so far:` (or `(planned)` for a dry run), so the
dated folders can be watched filling in rather than only read at the end. Directly
under the table, and outside its scrolling area so it always stays in view, a live
roll-up adds the years up - `Files per year so far:  2025: 1   2026: 340` - which
is the quickest read on a card that spans several years. The
counters row above it keeps its own live tally - `Imported 340   ·   duplicates
2   ·   failed 0   ·   sidecars 27` - including how many edit sidecars travelled
with their photos. Below it, the bytes written so far add up as the run goes -
`1.2 GB copied so far`, or `planned` on a dry run - so progress is visible by
volume as well as by file count, which matters when a card holds a few huge
videos among many small photos.

Beside the progress bar the window also estimates how much longer the run will
take - `about 3 min left` - from how fast the files done so far went. The
estimate appears once the first file has finished and disappears at the end, so
it never sits there claiming `0 s left`.

Shortcuts work anywhere in the window: **Ctrl+Enter** starts the import (or the
dry run, whichever the checkbox selects), and a plain **Enter** starts it too
when no path field has the keyboard, since Enter in a field belongs to the text
being typed. **Escape** cancels a run in progress after the file being copied. The log buttons answer to keys as well, so none of
them needs the mouse: **Ctrl+L** opens this run's log in your editor, **Ctrl+Shift+L**
shows it in Explorer with the file selected, **Ctrl+Shift+N** opens the newest log
in Notepad, **Ctrl+Shift+C** copies the last run's summary and **Ctrl+Shift+K**
copies the whole run log. These keys are also printed in the window itself, in
a dim line under the buttons, so they can be read without hovering anything; a
key whose button is not showing at that moment is left out of the line.

The log shortcuts are ignored while you are typing in a path field, where those
keys belong to the text; Ctrl+Enter is honoured either way, and Escape still
stops a run that is in progress even with a path field focused, since a field
only loses focus when nothing is being imported. A log
shortcut does nothing when the button it stands for is greyed out, so it cannot
open a log that does not exist or copy a summary no run has produced.

After the run, the day table is followed by a `Files per year:` line that adds
those days up by year (`2025: 1   2026: 36`), so a library spanning several years
reads at a glance.

## Sidecar files

Edits stored next to a photo travel with it: `.xmp` (Lightroom / Camera Raw),
`.aae` (iPhone adjustments), `.pp3` (RawTherapee) and `.dop` (DxO). Both naming
conventions are matched, case-insensitively: `IMG_0231.CR3.xmp` and
`IMG_0231.xmp` / `IMG_0231.AAE`.

- Sidecars land in the same dated folder as their photo and are renamed with it
  when a name clash adds `_1` (`IMG_0231_1.CR3` keeps `IMG_0231_1.CR3.xmp`).
- Moving copies them across too, and removes the originals.
- A sidecar is picked up when its photo is imported, or when the photo is already
  in the library and the sidecar is missing - so edits made *after* an import are
  carried over by re-running it.
- Sidecars without a matching photo in the same folder, and sidecars whose target
  already exists, are left alone.
- Dry runs show the sidecars they would carry without copying them.
- Sidecar lines in the log carry the sidecar's own size, just like the photo
  lines do.

Sidecars are matched within the folder of their photo, so a tree holding two
cards that both contain `IMG_0001.CR3` never swaps their edits. When a photo has
no sidecar in its own folder but a matching one turns up elsewhere in the
folders being imported, the run ends with a `Warning:` block naming both files.
Those edits are deliberately not copied across - the warning tells you where
they are so you can put them next to the photo and re-run.

## How the date is chosen

1. **EXIF `DateTimeOriginal`** - the moment the shutter was released.
2. EXIF `DateTimeDigitized`, then the EXIF `DateTime` tag in IFD0.
3. **XMP sidecar**: the capture date from `photo.xmp` or `photo.RAW.xmp` beside
   the picture - `exif:DateTimeOriginal` first, then `xmp:CreateDate`,
   `photoshop:DateCreated` and `tiff:DateTime` - for photos whose own file has no
   EXIF date. Any time zone or fractional second in the value is dropped and the
   wall-clock time is used as written.
4. **File creation time** (Windows), then the modification time, for images with
   no usable metadata - PNG/BMP/GIF exports, screenshots, files saved by editors.
5. Nothing at all readable: the current time, so the file still gets a home.

Values that camera firmware writes for "unknown" (for example
`0000:00:00 00:00:00`) are rejected and fall through to the file date, so a
broken tag cannot fill your library with a `0000` folder.

The EXIF timestamp is the camera's local wall-clock time and is used verbatim -
no time-zone shifting, so photos land in the folder matching the date they show.

Video headers store UTC, so that recording time is converted to local time before
the folder is picked - unlike EXIF, which is already local wall-clock time.

RAW containers the EXIF reader cannot open directly (`cr3`, `raf`, some TIFF
variants) are handled by locating the embedded TIFF/EXIF block inside the file,
so their shooting dates are read too.

## Duplicates and name clashes

- If the destination already contains a byte-identical file at that dated path,
  the file counts as a **duplicate** and is skipped - re-running an import over
  the same card is safe.
- If the name exists but the contents differ, both are kept and the new file gets
  a suffix: `IMG_0001.JPG`, `IMG_0001_1.JPG`, ...
- Nothing in the destination is ever overwritten.

## Copy vs. move

- **Copy** (default) leaves the originals untouched and preserves their modified
  and accessed timestamps on the copy.
- **Move** removes each original once its copy is safely in place. A move across
  volumes (`C:` -> `D:`) falls back to copy-then-delete. If the copy lands but
  the original cannot be deleted - a locked file, a read-only card - the log says
  so plainly instead of calling the file failed: `! Copied IMG_0042.JPG to
  2026/2026-06-01/IMG_0042.JPG, but could not delete it from the source: Access
  is denied. (os error 5)`. The file counts as imported, because it is in the
  library; only the original has been left behind.

Runs on a background thread with live progress and a log, and **Cancel** stops
after the file that is currently being handled.

## Drag and drop

Drop a folder onto the **Source folder** field to use it as the source, or onto
the **Destination** field to use it as the destination; a drop anywhere else in
the window is ignored rather than guessed at. Once a source is already set,
dropping another folder on the source field queues it alongside the first
instead of replacing it. Dropping a photo works too: the folder that holds it is
used, so dragging a picture straight out of an Explorer file list does what you
expect, and the log notes which file pointed at the folder. Only the first
dropped item is used, and the new path is remembered like any other.

Dropping several items at once is fine: every folder the drop came from gets a
place, in the order dropped, with repeats collapsed - photos from one folder
count once. The first folder fills the source field when it is empty, and the
rest queue behind it. The destination is a single folder, so it takes the first
of the drop and says in the log how many it ignored.

A dropped item with no reachable folder - a broken shortcut, or a path whose
folder has been deleted - is reported in the log and ignored.

## Several source folders

**+ Add folder…** under the source row queues more folders for the same run; each
queued folder has a tick box to pause it, **↑** and **↓** buttons to move it up or
down the list and a **✕** to take it out again, and the row shows how many are
queued (and how many are paused). Selecting several folders in the picker queues them all at
once.

Unticking a queued folder skips it for the run without forgetting it: it stays in
place, is remembered with the settings, and its files are simply left alone until
you tick it again. Paused rows are dimmed and marked `(paused)`, and are not
counted as source folders.

All of them are scanned recursively into the one destination: one log, one
summary, one duplicate check. A folder queued inside another source is imported
once, and the queue is remembered with the rest of the settings.

Folders are imported in the order they are listed, the top one first, so moving
a folder to the top makes it the one the run starts with. The end of the run
prints a `Files per source folder:` table with the count from each one. Within a single folder
the files are handled in name order either way, so a run remains repeatable.

## Opening the destination

**Open folder** in the toolbar opens the destination in File Explorer. It is
enabled as soon as the folder exists, so it is ready the moment a run finishes.

If the destination is the same folder as a source, sits inside one, or contains
one, the app says so under the path fields before the run starts and repeats the
warning at the top of the log. Every source folder that clashes is named, not only
the first one found. Ticking **Refuse to start if the destination
overlaps a source folder** turns all three of those clashes from warnings into a
refusal: the overlap is named - same folder, destination inside a source, or a
source inside the destination - and nothing is copied. While the tick is on and
the folders clash, **Start** is greyed out, and its hint names the overlap before
anything is pressed; Ctrl+Enter, which skips the button, still reports the same
refusal as an error. A source folder that is not on disk greys **Start** whatever
the tick says, naming every missing folder - a mistyped path, a deleted folder or
an unplugged card - and Ctrl+Enter puts the same sentence in the error line. The
tick is remembered as `refuse_overlap=1`, and Reset
leaves it alone. Without the tick the run still goes ahead - it is sometimes what
you want - and the destination subtree is skipped while walking, so an import can
never feed on its own output. For the same reason a source folder that sits inside
the destination is skipped outright: everything in it is already the library, or
output this app filed into it, and the log names the folder it left alone.

The destination drive's free space is weighed against the size of the pictures
and sidecars waiting to be copied as soon as a destination folder is picked -
by Browse, by drag and drop, or by finishing a typed path - and again when a run
starts. The same reading appears under the path fields as `E: 412.0 GB free`,
refreshed about once a second while an import runs, so the space being used up is
visible as files land. If the drive looks too small
for them, a second warning line says so - `E: has 3.2 GB free but about 4.6 GB
of photos and sidecars to copy; the run may run out of space.` - and the run
still starts if you go ahead. A dry run is never warned about space, since it
copies nothing.

## Dry run

Tick **Dry run** (the button becomes **Preview plan**) to see where every file
would land without changing anything. The tick is remembered between launches,
so a session left in dry run reopens in dry run - Reset does not clear it, since
it forgets folders rather than how careful you want to be: no folder is created in the destination,
no file is copied or moved, and nothing is deleted. Planned destinations are
listed in the log with the date source that chose them, and files already in the
library are still reported as duplicates, so the preview matches the real run.

## Run logs

Every import writes its log into the destination folder, alongside the dated
directories:

```
E:\Library\import-log-2026-11-06_204512.txt
```

The file receives the same lines as the Log pane - one per file with its
destination, size, the date it was filed under and the source that date came
from, plus duplicates, failures and the final summary - after a header recording
the start time, source, destination and Copy/Move mode: ```
+ 2024\DCIM\100CANON\IMG_0231.JPG  →  2026/2026-06-01/IMG_0231.JPG   4.2 MB   2026-06-01 10:22:33   [EXIF shooting date]
```

Skipped duplicates (`=`) and failures (`!`) carry the same size and resolved date,
so a log explains every file it touched, not only the ones it moved.

The summary also states the span of dates the run covered, from the oldest dated
folder to the newest - `Dates:  2019-04-02 to 2026-06-02 (over 412 days)`, or a
single date when everything landed in one folder - so a glance shows how far
apart the pictures in a batch are. With more than one source folder, a table
follows giving each folder's imported, duplicate and failed counts and their
total, so every file in every source is accounted for.

If folders in the queue were paused, the log names the ones that never ran, with
the number of photos, videos and sidecars each of them holds:
`Skipped 2 source folders because they were paused: E:\card2 (1204 files),
F:\card3 (86 files).` Working that out walks each paused folder, so the note
appears a moment after the run's totals. The same note goes into the **Copy
summary** text, so a pasted summary shows the run did not cover everything in the
queue, and what went with it.

Pausing *every* folder is caught before the run starts, and says so rather than
blaming an empty folder field - naming the folders so it is obvious which ticks
to move: `Nothing to import: all 3 source folders are paused: D:\card, E:\card2,
F:\card3. Untick one in the list to include it.` With a single queued folder the
wording is `the only source folder is paused: D:\card.`

**Unpause all**, beside **+ Add folder…** above the queue, ticks every paused
folder back on in one click - handy after a run that skipped several - and is
greyed out when nothing is paused. The folders keep their places in the list.

**+ Add folder…** takes a multiple selection from the folder dialog: Ctrl-click
several folders and they all arrive at once, the first filling the source field
when it is empty and the rest queuing behind it. Repeats - including a folder
already in the field - are left out, and the status line says how many were
added and how many were already there.

The closing line adds up the run, including how much was copied:
`Done: 128 imported (4.6 GB), 3 duplicates skipped, 0 failed in 42.1s`. Dry runs
report the size they would copy.

Lines are written as the run progresses, so the log survives even
if the app is closed mid-import. Each run starts a new file; a second run in the
same second gets `_2`, `_3` instead of overwriting. Only the newest run logs are kept -
5 unless you change it: the **Keep [5] logs** control in the toolbar sets the
count, which is remembered between launches. Lowering it deletes the surplus
logs at once, so the destination is trimmed without waiting for the next run;
creating a new log also deletes older `import-log-*.txt` files beyond the count
so the destination does not fill up with text files. Nothing else in that folder is touched.

**Delete logs** in the toolbar removes every run log in the destination in one
go, for when the record of past runs is not worth keeping. It deletes the same
`import-log-*.txt` files pruning does - photos, sidecars and every other file in
that folder stay exactly where they are. The timestamped log is a
`.txt`, so the importer never picks it up again.

Dry runs deliberately do not create a log file: a preview promises to change
nothing in the destination.

The log buttons appear only when they have something to work on - no log yet
means no **Open log**, **Show log**, **Copy log** or **Newest log in Notepad**,
and no finished run means no **Copy summary** - so the toolbar carries no dead
controls. Keyboard shortcuts for them do nothing while they are hidden.

**Editor:** beside them sets the program Newest log opens - a file picker for a
`.exe`, and the button is labelled `Editor: Notepad` until you pick one, then
with that program's name. The choice is remembered as `log_editor=` in
`settings.txt`, and Reset leaves it alone.

**Open log** in the toolbar opens this run's log file in whichever program is
registered for `.txt` files - Notepad on a stock Windows, your own editor if you
have changed it - and **Show log** opens Explorer with that file selected, ready
to rename, copy or send on. **Copy summary** puts the last run's totals and
tables on the clipboard as plain text, ready to paste into a message or note,
while **Copy log** copies the entire run log - the totals, the tables and every
file line - for when the details matter more than the tally. **Newest log in Notepad** (renamed to your own program once you choose one)
ignores the `.txt` association entirely: it finds the latest `import-log-*.txt` in the
destination and hands it straight to Notepad, which is what you want when the
association points at a heavy editor. It stays greyed
out until a run has actually written one, and it is cleared when a dry run starts
so it never points at an older run's file.

The queue's order is set with the **↑** and **↓** buttons beside each folder, so a
run's order can be arranged without dragging anything.

Queued folders are numbered in import order - `2.`, `3.`, ... - with the source
field counting as 1, so the run's order is visible at a glance. A paused folder
keeps its number, because it keeps its place in the list.

## Remembered folders, mode and window

The source folder, destination folder and Copy/Move choice you last used are
restored on the next launch, together with the size, position and maximized
state of the window.
They are stored in `%APPDATA%\ImageImporter\settings.txt` as `key=value` lines,
written whenever a folder is picked, a path is edited, the mode is switched or
the window is moved:

```
source=D:\Photos 2024\DCIM
destination=E:\Library
mode=move
window_size=1280x800
window_position=120,80
maximized=1
```

Copy is the default, so `mode=move` appears only when Move is selected; a missing
`mode` line - or an unrecognised value - means Copy. Clearing a path field
forgets that path. A window size below the minimum, or a position far outside any
screen, is ignored, so detaching a monitor cannot open the app off-screen.

`maximized=1` is written only when you leave the window maximized, and the app
then reopens maximized. The size and position stored alongside it are the last
*normal* placement, so un-maximizing puts the window back where it was instead of
to a full-screen size hugging the corner of the monitor.

Paths can be typed, pasted with **Ctrl+V** or dropped from Explorer. Explorer's
*Copy as path* arrives quoted (`"D:\Photos"`), so the quotes and any stray spaces
are stripped when the field loses focus - pasting straight from the address bar
needs no cleanup by hand. Pasting the path of a photo or video, copied out of a
file listing, fills the field with the folder that holds it, because that is what
an import reads; a path ending in a known photo or video extension is treated the
same way even before the file exists. Folders and unrelated files are left as
typed. A filled-in field whose folder is not there is flagged underneath it -
`⚠ No folder at D:\card\DCIM — click to create it` - so a typo or an unplugged
card shows up before Start. Clicking the note makes the folder, so a destination
typed fresh on a mounted drive takes one click; a path that cannot be made - a
drive that is not there, or a file in the way - says why in the error line instead.
A destination that does not exist yet is not flagged: the first run creates it, and
only a path with nothing above it on disk earns the warning.

Each path field has a **Recent** dropdown listing the folders last used for that
field, most recent first, up to eight. Pressing **Enter** in a field files the
typed folder there at once - tidied, but without starting anything - so a folder
tried and dismissed is still offered next time. The destination's dropdown also
lists the folders used as sources, since a folder often serves as both: its own
history comes first, then source folders it has not seen, with pinned favourites
ahead of everything. Only folders that still exist are offered. Choosing one fills the field; entries whose
folder no longer exists are left out of the list. A folder joins the list when an
import starts, and the history is stored in the same file as `recent_source=` and
`recent_destination=` lines.

Right-clicking an entry in the dropdown removes that folder from the list (and
from the favourites), for folders you no longer want suggested.

The **☆** button beside a path pins the folder in that field: pinned folders are
marked **★** in the dropdown, are listed first, and are never pushed out by newer
ones - only unpinned entries make room. A folder pinned once is a favourite for
both lists, stored as `pinned=` lines, and **Reset** keeps the pins while it
clears the recents.

**Reset** in the toolbar forgets the remembered paths, mode and recent lists: both
fields are emptied, the mode returns to Copy and the dropdowns become empty. The
window placement is kept, so the app still opens where you left it.

## For contributors and AI agents

[`README.llm.md`](README.llm.md) is an engineering guide to this codebase:
module map, event flow, import invariants, settings format, test conventions and
recipes for common changes. Read it before changing code; this file documents
what the app does, that one documents how it is built.

## Build

Requires a Rust toolchain with a working Windows linker; this repo pins the GNU
target in `.cargo/config.toml` (see *Toolchain* below).

```bash
cargo build --release
cargo test
```

The executable lands at:

```
target/x86_64-pc-windows-gnu/release/image_importer.exe
```

Run it by double-clicking the exe or with `cargo run --release`. Release builds
are GUI-only (no console window).

## Toolchain

This machine has no Windows SDK import libraries, which the default MSVC target
needs to link, so the project builds with the GNU toolchain instead:

- `.cargo/config.toml` pins `target = "x86_64-pc-windows-gnu"`.
- A project-scoped `rustup override set stable-x86_64-pc-windows-gnu` is active
  for this folder, so plain `cargo build` uses that toolchain.
- The GNU target links with mingw-w64 in `C:\ProgramData\mingw64\mingw64\bin`,
  which the installer added to `PATH`; open a new terminal if it is missing.

To build against MSVC instead, install the *Desktop development with C++*
workload plus a Windows SDK, then delete `.cargo/config.toml` and run
`rustup override unset`.
