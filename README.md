# Photo Picker

A fast photo culling app for JPEGs straight off an SD card, and for iPhone HEIC photos. It runs
natively on macOS (Apple Silicon) and Windows 11 from the same code.

Cull in two passes if you like: press **S** to shortlist photos that are roughly good (★★★, 3 stars),
then use the Shortlist filter and press **P** to pick the best (★★★★, 4 stars). **X** rejects
(★, 1 star). Only picks get the macOS Finder tag "Picked". The rating is
written into the JPEG straight away, so Lightroom, Bridge, Capture One, darktable, digiKam,
Windows Explorer and others see it.

## Build & run

Install Rust from https://rustup.rs, then:

```sh
cargo build --release
# macOS
./target/release/photo-picker /Volumes/SDCARD
# Windows
target\release\photo-picker.exe E:\
```

You can also start it without arguments and open a folder with Ctrl/Cmd+O, or drag a folder onto
the window. Subfolders are scanned too (e.g. `DCIM/100MSDCF`).

Photos are shown in the order they were taken (EXIF DateTimeOriginal; photos without one use the
file's modification time and are marked "file date" in the info box). Until all dates are read,
they appear in folder/file name order. The bar under the toolbar has one entry per month with its photo
count, so you can cull a big year folder month by month; months with nothing left unrated are
dimmed. The month filter combines with the rating filter (e.g. Unrated in March).

To build a double-clickable Mac app with the icon, run `scripts/bundle-mac.sh`; it creates
`dist/Photo Picker.app`. The icon is drawn by `assets/make_icon.py` (output: `assets/icon.png`).

On Windows, run `powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1` to get
`dist\Photo Picker.exe` with the icon embedded; add `-Shortcut` to also put it in the Start menu.
The `.ico` is made from `assets/icon.png` by `scripts/make-ico.ps1`.

## Keys

| Key | Action |
|---|---|
| **S** | Shortlist (rough pick) → 3 stars; on a shortlisted photo → 1 star |
| **P** | Pick → 4 stars; on a picked photo, unpick → 1 star |
| **X** / **U** | Unpick / reject → 1 star |
| **1 … 5** | Set 1–5 stars |
| **0** | Clear rating |
| **T** | Add or remove the family keyword (default "pere", JPEG only) |
| → ↓ Space / ← ↑ Backspace | Next / previous |
| PgDn / PgUp, Home / End | Jump 10, first / last |
| **Z**, Enter, click | 100% zoom where you clicked (drag to pan) |
| **F** | Filter: All / Unrated / Shortlist (★3+) / Picked / Rejected |
| **[** / **]**, Shift+← / Shift+→ | Previous / next month |
| **M** | All months |
| **A** | Toggle auto-advance after rating (on by default) |
| **I** | Toggle photo info (camera, lens, exposure) |
| H / ? / F1 | Help |

**Copy N picked…** (toolbar) copies every photo rated ★4 or higher to a folder you choose (from
all months, whatever month is selected), either into that one folder or into month subfolders
(`2016-03`, `2016-04`, …). Copies keep the rating (HEIC sidecars are copied along) and the original
modification time. A file that is already at the destination (same name, size and modification
time) is skipped; a different photo with the same name, as iPhones reuse `IMG_1234`, is copied as
`IMG_1234_1.HEIC`.

## Keywords (sharing with Synology Photos)

**T** adds a keyword to the current JPEG, or removes it if it's already there; it doesn't move on to
the next photo, so you can still rate it. Photos with the keyword show a badge and a dot in the filmstrip. Synology
Photos reads it as a tag, so a conditional album filtered on that tag fills up by itself.

The keyword is `pere` unless changed in the settings file, which is created on first run. Restart
the app after editing it. It may be up to 64 bytes (õ, ä, ö, ü take two).

* macOS: `~/Library/Application Support/Photo Picker/settings.txt`
* Windows: `%APPDATA%\Photo Picker\settings.txt`

Keywords are read from the file on load (XMP first, then IPTC, as Synology does). Matching ignores
case and Unicode form, so `Pere` is not duplicated. HEIC and RAF files aren't supported.

## HEIC (iPhone photos)

HEIC files are shown, sorted by date and rated like JPEGs, but **never modified**: the rating goes to
a small sidecar file next to the photo, `IMG_1234.HEIC.xmp` (standard XMP `xmp:Rating`, the naming
darktable and digiKam use), so the app remembers it between sessions.

Decoding uses the operating system:
* **Windows 11:** needs "HEIF Image Extensions" and "HEVC Video Extensions" from the Microsoft
  Store. If the Photos app opens iPhone photos, both are installed. Otherwise the app says so.
* **macOS:** built in.

## What gets written

For JPEG:

* EXIF IFD0 `Rating` (0x4746) and `RatingPercent` (0x4749): the Windows convention
  (1→1, 2→25, 3→50, 4→75, 5→99).
* XMP `xmp:Rating` (plus `MicrosoftPhoto:Rating` if it was already there): the Adobe
  convention.
* macOS only: a **Picked** Finder tag on photos rated ★4 or more, removed again when the rating
  drops. Other Finder tags and extended attributes on the file are kept. Search for it in Finder
  or Spotlight with `tag:Picked`.

The keyword goes to XMP `dc:subject` and IPTC `Keywords` (2:25). Other keywords keep their order.
XMP wins in Synology, so when one of the two doesn't exist yet it starts with the other's keywords.
IPTC is marked as UTF-8 (1:90): text already there in another encoding is converted first, and a
stored IPTC digest is updated. The photo is refused with an error, and left unchanged, if it uses
something this can't edit safely: an unsupported `dc:subject` form, a non-UTF-8 IPTC character set,
or metadata after an MPF (multi-picture) segment, which would break the offsets stored there.

Safety:
* Only the metadata segments change. The compressed image data is copied byte for byte.
* Existing EXIF data (including maker notes) is never moved.
* Each write goes to a temp file first, is re-read to verify, and then atomically replaces the
  original.
* The file's modification and creation times and its permissions are preserved.
* Keyword writes also compare every other segment and all other keywords before replacing the file.

## Performance notes

* On Apple Silicon, `.cargo/config.toml` builds for the CPU of the Mac that compiles it
  (`target-cpu=native`). Don't copy that binary to an older Mac; rebuild there instead.
* On Windows (x86_64) the same applies: the build uses the compiling PC's CPU features (AVX2 etc.)
  and links the C runtime statically, so the `.exe` needs no VC++ redistributable but may not
  run on an older CPU. Rebuild on the target PC instead.
* Previews are decoded at about screen size using JPEG DCT scaling (1/2, 1/4, 1/8). When a
  full-size decode is needed, the faster zune-jpeg decoder is used.
* Up to 8 worker threads prefetch the next ~6 and previous ~3 photos in the direction you are
  moving, most important first. Stale work is dropped when you jump ahead.
* The embedded EXIF thumbnail is shown instantly while the full preview decodes.
* Writes happen on a background thread, so rating never blocks navigation. Quitting waits for
  pending writes.

Tests: `cargo test`. To run the rating roundtrip and a decode benchmark against real files
(**use copies — this rewrites ratings**):
`PICKER_TEST_DIR=/path/to/copies cargo test --release -- --ignored --nocapture`.
This also runs a keyword roundtrip compared against `exiftool`, which the tests use when it's
installed (`brew install exiftool`). **It leaves the keyword on the copies.**
