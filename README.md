# Photo Picker

A fast photo culling app for JPEGs straight off an SD card. It runs natively on macOS (Apple Silicon)
and Windows 11 from the same code.

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
| → ↓ Space / ← ↑ Backspace | Next / previous |
| PgDn / PgUp, Home / End | Jump 10, first / last |
| **Z**, Enter, click | 100% zoom where you clicked (drag to pan) |
| **F** | Filter: All / Unrated / Shortlist (★3+) / Picked / Rejected |
| **A** | Toggle auto-advance after rating (on by default) |
| **I** | Toggle photo info (camera, lens, exposure) |
| H / ? / F1 | Help |

**Copy N picked…** (toolbar) copies every photo rated ★4 or higher to a folder you choose.
Copies keep the rating and the original modification time. Files already at the destination are
skipped.

## What gets written

* EXIF IFD0 `Rating` (0x4746) and `RatingPercent` (0x4749): the Windows convention
  (1→1, 2→25, 3→50, 4→75, 5→99).
* XMP `xmp:Rating` (plus `MicrosoftPhoto:Rating` if it was already there): the Adobe
  convention.
* macOS only: a **Picked** Finder tag on photos rated ★4 or more, removed again when the rating
  drops. Other Finder tags and extended attributes on the file are kept. Search for it in Finder
  or Spotlight with `tag:Picked`.

Safety:
* Only the metadata segments change. The compressed image data is copied byte for byte.
* Existing EXIF data (including maker notes) is never moved.
* Each write goes to a temp file first, is re-read to verify, and then atomically replaces the
  original.
* The file's modification time is preserved.

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
`PICKER_TEST_DIR=/path/to/copies cargo test --release -- --ignored --nocapture`
