//! HEIC / HEIF photos (what iPhones save): metadata is read from the ISO-BMFF
//! container here; decoding uses the operating system (ImageIO on macOS, WIC on
//! Windows). The files themselves are never modified: ratings go to an XMP
//! sidecar next to the photo (see `meta::write_rating`).

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::meta::{self, Meta};

pub fn is_heic(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("heic" | "heif" | "hif")
    )
}

/// Reads EXIF, XMP, size and orientation of the primary image, plus the
/// rating from the sidecar if there is one.
pub fn read_meta(path: &Path) -> io::Result<Meta> {
    let mut f = io::BufReader::with_capacity(64 * 1024, fs::File::open(path)?);
    let info = parse(&mut f)?;
    let mut app1s = Vec::new();
    if let Some(tiff) = info.exif {
        app1s.push([meta::EXIF_HEADER, &tiff].concat());
    }
    if let Some(xmp) = info.xmp {
        app1s.push([meta::XMP_HEADER, &xmp].concat());
    }
    let mut m = meta::parse_app1s(app1s.iter().map(|v| v.as_slice()));
    // HEIF stores the rotation in the container (irot/imir); an EXIF
    // orientation inside is informative only and must not be applied again.
    m.orientation = info.orientation;
    m.size = info.size.map(|(w, h)| if info.orientation >= 5 { (h, w) } else { (w, h) });
    m.thumbnail = None;
    if let Some(r) = meta::read_sidecar_rating(path) {
        m.rating = Some(r);
    }
    Ok(m)
}

#[derive(Default, Debug)]
struct Info {
    /// EXIF as a TIFF block (without the "Exif\0\0" header).
    exif: Option<Vec<u8>>,
    xmp: Option<Vec<u8>>,
    /// Stored size of the primary image, before rotation.
    size: Option<(u32, u32)>,
    /// The container's rotation/mirroring as an EXIF orientation (1..=8).
    orientation: u8,
}

/// Item data is never larger than this (EXIF and XMP are a few KB).
const MAX_ITEM: u64 = 4 << 20;
const MAX_META: u64 = 16 << 20;

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Finds the top-level `meta` box and pulls the bits we need out of it.
fn parse<R: Read + Seek>(f: &mut R) -> io::Result<Info> {
    let file_len = f.seek(SeekFrom::End(0))?;
    f.seek(SeekFrom::Start(0))?;
    let mut pos = 0u64;
    let meta_box = loop {
        if pos + 8 > file_len {
            return Err(invalid("no HEIF meta box"));
        }
        f.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 16];
        f.read_exact(&mut h[..8])?;
        let mut size = u32::from_be_bytes(h[0..4].try_into().unwrap()) as u64;
        let typ: [u8; 4] = h[4..8].try_into().unwrap();
        let mut hdr = 8;
        if size == 1 {
            f.read_exact(&mut h[8..16])?;
            size = u64::from_be_bytes(h[8..16].try_into().unwrap());
            hdr = 16;
        } else if size == 0 {
            size = file_len - pos;
        }
        if size < hdr {
            return Err(invalid("corrupt HEIF box"));
        }
        if pos == 0 && &typ != b"ftyp" {
            return Err(invalid("not a HEIF file"));
        }
        if &typ == b"meta" {
            if size - hdr > MAX_META {
                return Err(invalid("HEIF meta box too large"));
            }
            let mut body = vec![0u8; (size - hdr) as usize];
            f.read_exact(&mut body)?;
            break body;
        }
        pos += size;
    };
    // `meta` is a FullBox: skip version and flags.
    let body = meta_box.get(4..).ok_or_else(|| invalid("short HEIF meta box"))?;

    let mut primary = None;
    let mut items: Vec<(u32, [u8; 4], String)> = Vec::new(); // id, type, MIME content type
    let mut locs: Vec<Loc> = Vec::new();
    let mut idat: &[u8] = &[];
    let mut props: Vec<(&[u8; 4], &[u8])> = Vec::new();
    let mut assoc: Vec<(u32, Vec<u16>)> = Vec::new();
    for (typ, b) in Boxes(body) {
        match typ {
            b"pitm" => primary = full_box(b).and_then(|(v, d)| if v == 0 { be(d, 0, 2) } else { be(d, 0, 4) }),
            b"iinf" => {
                let Some((v, d)) = full_box(b) else { continue };
                let skip = if v == 0 { 2 } else { 4 };
                for (t, infe) in Boxes(d.get(skip..).unwrap_or(&[])) {
                    if t != b"infe" {
                        continue;
                    }
                    if let Some(item) = parse_infe(infe) {
                        items.push(item);
                    }
                }
            }
            b"iloc" => locs = parse_iloc(b).unwrap_or_default(),
            b"idat" => idat = b,
            b"iprp" => {
                for (t, d) in Boxes(b) {
                    match t {
                        b"ipco" => props = Boxes(d).collect(),
                        b"ipma" => assoc = parse_ipma(d).unwrap_or_default(),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    let mut info = Info { orientation: 1, ..Default::default() };
    if let Some(primary) = primary {
        let (mut rot, mut mirror) = (0u8, None);
        for (_, idxs) in assoc.iter().filter(|(id, _)| *id == primary) {
            for &i in idxs {
                // Property indices are 1-based; 0 means "none".
                let Some(&(t, d)) = (i as usize).checked_sub(1).and_then(|i| props.get(i)) else { continue };
                match t {
                    b"ispe" => {
                        if let Some((_, d)) = full_box(d) {
                            if let (Some(w), Some(h)) = (be(d, 0, 4), be(d, 4, 4)) {
                                info.size = Some((w, h));
                            }
                        }
                    }
                    b"irot" => rot = d.first().map_or(0, |v| v & 3),
                    b"imir" => mirror = d.first().map(|v| v & 1),
                    _ => {}
                }
            }
        }
        info.orientation = orientation(rot, mirror);
    }

    let mut read_item = |id: u32| -> Option<Vec<u8>> {
        let loc = locs.iter().find(|l| l.id == id)?;
        let mut out = Vec::new();
        for &(off, len) in &loc.extents {
            if out.len() as u64 + len > MAX_ITEM {
                return None;
            }
            let start = loc.base.checked_add(off)?;
            if loc.method == 1 {
                let (s, e) = (start as usize, (start + len) as usize);
                out.extend_from_slice(idat.get(s..e)?);
            } else if loc.method == 0 {
                // A length of 0 means "to the end of the file"; never the case for EXIF/XMP.
                if len == 0 || start + len > file_len {
                    return None;
                }
                f.seek(SeekFrom::Start(start)).ok()?;
                let mut buf = vec![0u8; len as usize];
                f.read_exact(&mut buf).ok()?;
                out.extend_from_slice(&buf);
            } else {
                return None;
            }
        }
        Some(out)
    };
    for (id, typ, content_type) in &items {
        if typ == b"Exif" && info.exif.is_none() {
            // The item starts with the offset of the TIFF header (usually 6, past "Exif\0\0").
            info.exif = read_item(*id).and_then(|d| {
                let skip = be(&d, 0, 4)? as usize;
                d.get(4 + skip..).map(|t| t.to_vec())
            });
        } else if typ == b"mime" && content_type == "application/rdf+xml" && info.xmp.is_none() {
            info.xmp = read_item(*id);
        }
    }
    Ok(info)
}

/// HEIF `irot` (counter-clockwise quarter turns) and `imir` (0: mirror about
/// the vertical axis, i.e. left/right; 1: about the horizontal axis) as an
/// EXIF orientation. Mirroring combined with rotation is rare (iPhones never
/// write it) and only the rotation is kept then.
fn orientation(rot: u8, mirror: Option<u8>) -> u8 {
    match (rot, mirror) {
        (0, Some(0)) => 2,
        (0, Some(_)) => 4,
        (0, None) => 1,
        (1, _) => 8,
        (2, _) => 3,
        _ => 6,
    }
}

struct Loc {
    id: u32,
    /// 0: offsets are file offsets; 1: offsets are into `idat`.
    method: u8,
    base: u64,
    extents: Vec<(u64, u64)>,
}

/// Big-endian unsigned integer of `n` (0, 2, 4 or 8) bytes at `at`.
fn be(d: &[u8], at: usize, n: usize) -> Option<u32> {
    be64(d, at, n).and_then(|v| u32::try_from(v).ok())
}

fn be64(d: &[u8], at: usize, n: usize) -> Option<u64> {
    let b = d.get(at..at.checked_add(n)?)?;
    Some(b.iter().fold(0u64, |acc, &x| (acc << 8) | x as u64))
}

/// Splits a FullBox payload into (version, rest).
fn full_box(b: &[u8]) -> Option<(u8, &[u8])> {
    Some((*b.first()?, b.get(4..)?))
}

fn parse_infe(b: &[u8]) -> Option<(u32, [u8; 4], String)> {
    let (v, d) = full_box(b)?;
    if v < 2 {
        return None; // Old item info entries have no item type.
    }
    let (id, at) = if v == 2 { (be(d, 0, 2)?, 2) } else { (be(d, 0, 4)?, 4) };
    let typ: [u8; 4] = d.get(at + 2..at + 6)?.try_into().ok()?;
    let mut content_type = String::new();
    if &typ == b"mime" {
        // item_name\0 content_type\0 ...
        let rest = d.get(at + 6..)?;
        let mut parts = rest.split(|&c| c == 0);
        parts.next();
        content_type = String::from_utf8_lossy(parts.next().unwrap_or(&[])).into_owned();
    }
    Some((id, typ, content_type))
}

fn parse_iloc(b: &[u8]) -> Option<Vec<Loc>> {
    let (v, d) = full_box(b)?;
    let (off_size, len_size) = ((d.first()? >> 4) as usize, (d.first()? & 15) as usize);
    let (base_size, index_size) = ((d.get(1)? >> 4) as usize, if v >= 1 { (d.get(1)? & 15) as usize } else { 0 });
    let mut at = 2;
    let count = if v < 2 { be(d, at, 2)? } else { be(d, at, 4)? };
    at += if v < 2 { 2 } else { 4 };
    let mut out = Vec::new();
    for _ in 0..count {
        let id = if v < 2 { be(d, at, 2)? } else { be(d, at, 4)? };
        at += if v < 2 { 2 } else { 4 };
        let mut method = 0;
        if v >= 1 {
            method = (be(d, at, 2)? & 15) as u8;
            at += 2;
        }
        at += 2; // data_reference_index
        let base = be64(d, at, base_size)?;
        at += base_size;
        let n = be(d, at, 2)?;
        at += 2;
        let mut extents = Vec::new();
        for _ in 0..n {
            at += index_size;
            let off = be64(d, at, off_size)?;
            at += off_size;
            let len = be64(d, at, len_size)?;
            at += len_size;
            extents.push((off, len));
        }
        out.push(Loc { id, method, base, extents });
    }
    Some(out)
}

fn parse_ipma(b: &[u8]) -> Option<Vec<(u32, Vec<u16>)>> {
    let (v, d) = full_box(b)?;
    let large = b.get(3)? & 1 == 1;
    let count = be(d, 0, 4)?;
    let mut at = 4;
    let mut out = Vec::new();
    for _ in 0..count {
        let id = if v < 1 { be(d, at, 2)? } else { be(d, at, 4)? };
        at += if v < 1 { 2 } else { 4 };
        let n = *d.get(at)? as usize;
        at += 1;
        let mut idxs = Vec::with_capacity(n);
        for _ in 0..n {
            // The top bit marks the property as essential.
            if large {
                idxs.push((be(d, at, 2)? & 0x7FFF) as u16);
                at += 2;
            } else {
                idxs.push((d.get(at)? & 0x7F) as u16);
                at += 1;
            }
        }
        out.push((id, idxs));
    }
    Some(out)
}

/// Iterates over the child boxes in `d`, yielding (type, payload).
struct Boxes<'a>(&'a [u8]);

impl<'a> Iterator for Boxes<'a> {
    type Item = (&'a [u8; 4], &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        let d = self.0;
        let mut size = be64(d, 0, 4)? as usize;
        let typ: &[u8; 4] = d.get(4..8)?.try_into().ok()?;
        let mut hdr = 8;
        if size == 1 {
            size = usize::try_from(be64(d, 8, 8)?).ok()?;
            hdr = 16;
        } else if size == 0 {
            size = d.len();
        }
        if size < hdr || size > d.len() {
            self.0 = &[];
            return None;
        }
        self.0 = &d[size..];
        Some((typ, &d[hdr..size]))
    }
}

// ---------------------------------------------------------------------------
// Decoding

/// Decodes a HEIC so the displayed (rotated) image fits `max_w` x `max_h`,
/// capped at `max_side` (the GPU texture limit). `thumb` allows the small
/// embedded preview image, which is much faster to decode.
#[cfg(target_os = "macos")]
pub fn decode(
    path: &Path,
    m: &Meta,
    thumb: bool,
    max_w: u32,
    max_h: u32,
    max_side: u32,
) -> Result<eframe::egui::ColorImage, String> {
    // ImageIO applies the HEIF rotation itself and scales while decoding.
    let longest = match m.size {
        Some((w, h)) => {
            let fit = (max_w as f64 / w as f64).min(max_h as f64 / h as f64).min(1.0);
            ((w.max(h) as f64 * fit).ceil() as u32).max(1)
        }
        None => max_w.max(max_h),
    };
    mac::decode(path, longest.min(max_side), thumb)
}

#[cfg(windows)]
pub fn decode(
    path: &Path,
    m: &Meta,
    thumb: bool,
    max_w: u32,
    max_h: u32,
    max_side: u32,
) -> Result<eframe::egui::ColorImage, String> {
    win::decode(path, m, thumb, max_w, max_h, max_side)
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn decode(
    _path: &Path,
    _m: &Meta,
    _thumb: bool,
    _max_w: u32,
    _max_h: u32,
    _max_side: u32,
) -> Result<eframe::egui::ColorImage, String> {
    Err("HEIC photos can't be shown on this system".into())
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use eframe::egui::{Color32, ColorImage};

    type CFRef = *const c_void;

    #[repr(C)]
    struct CGRect {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    #[link(name = "CoreGraphics", kind = "framework")]
    #[link(name = "ImageIO", kind = "framework")]
    unsafe extern "C" {
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;
        static kCFBooleanTrue: CFRef;
        static kCGImageSourceCreateThumbnailWithTransform: CFRef;
        static kCGImageSourceCreateThumbnailFromImageAlways: CFRef;
        static kCGImageSourceCreateThumbnailFromImageIfAbsent: CFRef;
        static kCGImageSourceThumbnailMaxPixelSize: CFRef;
        static kCGColorSpaceSRGB: CFRef;
        fn CFRelease(cf: CFRef);
        fn CFNumberCreate(alloc: CFRef, the_type: isize, value: *const c_void) -> CFRef;
        fn CFDictionaryCreate(
            alloc: CFRef,
            keys: *const CFRef,
            values: *const CFRef,
            count: isize,
            key_cb: *const c_void,
            value_cb: *const c_void,
        ) -> CFRef;
        fn CFURLCreateFromFileSystemRepresentation(alloc: CFRef, buf: *const u8, len: isize, is_dir: u8) -> CFRef;
        fn CGImageSourceCreateWithURL(url: CFRef, options: CFRef) -> CFRef;
        fn CGImageSourceCreateThumbnailAtIndex(src: CFRef, index: usize, options: CFRef) -> CFRef;
        fn CGImageGetWidth(img: CFRef) -> usize;
        fn CGImageGetHeight(img: CFRef) -> usize;
        fn CGColorSpaceCreateWithName(name: CFRef) -> CFRef;
        fn CGBitmapContextCreate(
            data: *mut c_void,
            w: usize,
            h: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: CFRef,
            info: u32,
        ) -> CFRef;
        fn CGContextDrawImage(ctx: CFRef, rect: CGRect, img: CFRef);
    }

    /// Releases a Core Foundation object when dropped.
    struct Owned(CFRef);
    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) };
            }
        }
    }
    fn owned(r: CFRef, what: &str) -> Result<Owned, String> {
        if r.is_null() { Err(format!("cannot read HEIC ({what})")) } else { Ok(Owned(r)) }
    }

    const K_CF_NUMBER_SINT64: isize = 4;
    const K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST: u32 = 1;

    pub fn decode(path: &Path, longest: u32, thumb: bool) -> Result<ColorImage, String> {
        unsafe {
            let p = path.as_os_str().as_bytes();
            let url = owned(CFURLCreateFromFileSystemRepresentation(std::ptr::null(), p.as_ptr(), p.len() as isize, 0), "path")?;
            let src = owned(CGImageSourceCreateWithURL(url.0, std::ptr::null()), "file")?;
            let max = longest as i64;
            let num = owned(CFNumberCreate(std::ptr::null(), K_CF_NUMBER_SINT64, &max as *const i64 as *const c_void), "size")?;
            // "IfAbsent" may use the embedded preview; "Always" decodes the full image.
            let from = if thumb { kCGImageSourceCreateThumbnailFromImageIfAbsent } else { kCGImageSourceCreateThumbnailFromImageAlways };
            let keys = [kCGImageSourceCreateThumbnailWithTransform, from, kCGImageSourceThumbnailMaxPixelSize];
            let values = [kCFBooleanTrue, kCFBooleanTrue, num.0];
            let opts = owned(
                CFDictionaryCreate(
                    std::ptr::null(),
                    keys.as_ptr(),
                    values.as_ptr(),
                    keys.len() as isize,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                ),
                "options",
            )?;
            let img = owned(CGImageSourceCreateThumbnailAtIndex(src.0, 0, opts.0), "decode")?;
            let (w, h) = (CGImageGetWidth(img.0), CGImageGetHeight(img.0));
            if w == 0 || h == 0 {
                return Err("empty HEIC image".into());
            }
            let mut pixels = vec![Color32::BLACK; w * h];
            let space = owned(CGColorSpaceCreateWithName(kCGColorSpaceSRGB), "color space")?;
            // Premultiplied RGBA bytes are exactly egui's Color32 layout.
            let ctx = owned(
                CGBitmapContextCreate(
                    pixels.as_mut_ptr() as *mut c_void,
                    w,
                    h,
                    8,
                    w * 4,
                    space.0,
                    K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST,
                ),
                "bitmap",
            )?;
            CGContextDrawImage(ctx.0, CGRect { x: 0.0, y: 0.0, w: w as f64, h: h as f64 }, img.0);
            drop(ctx);
            Ok(ColorImage::new([w, h], pixels))
        }
    }
}

#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use std::path::Path;
    use std::sync::atomic::{AtomicU8, Ordering};

    use eframe::egui::{Color32, ColorImage};
    use windows::Win32::Foundation::{GENERIC_READ, WINCODEC_ERR_COMPONENTNOTFOUND};
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
    use windows::core::{HSTRING, Interface};

    use crate::meta::Meta;

    const STORE_HINT: &str = "Windows can't decode HEIC photos yet: install \"HEIF Image Extensions\" and \
        \"HEVC Video Extensions\" from the Microsoft Store";

    thread_local! {
        static FACTORY: RefCell<Option<IWICImagingFactory>> = const { RefCell::new(None) };
    }

    /// Whether WIC already applies the HEIF rotation: 0 = unknown, 1 = yes, 2 = no.
    /// Learned from the first rotated photo whose decoded aspect ratio tells.
    static APPLIES_ROTATION: AtomicU8 = AtomicU8::new(0);

    fn factory() -> Result<IWICImagingFactory, String> {
        FACTORY.with(|f| {
            if let Some(f) = &*f.borrow() {
                return Ok(f.clone());
            }
            unsafe {
                // Fails harmlessly if COM is already initialized on this thread.
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let fac: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("Windows Imaging Component: {e}"))?;
                *f.borrow_mut() = Some(fac.clone());
                Ok(fac)
            }
        })
    }

    pub fn decode(path: &Path, m: &Meta, thumb: bool, max_w: u32, max_h: u32, max_side: u32) -> Result<ColorImage, String> {
        let fac = factory()?;
        unsafe {
            let dec = fac
                .CreateDecoderFromFilename(&HSTRING::from(path.as_os_str()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)
                .map_err(|e| if e.code() == WINCODEC_ERR_COMPONENTNOTFOUND { STORE_HINT.to_string() } else { e.to_string() })?;
            let frame = dec.GetFrame(0).map_err(|e| e.to_string())?;
            let source: IWICBitmapSource = match thumb.then(|| frame.GetThumbnail().ok()).flatten() {
                Some(t) => t,
                None => frame.cast().map_err(|e| e.to_string())?,
            };
            let (mut w, mut h) = (0u32, 0u32);
            source.GetSize(&mut w, &mut h).map_err(|e| e.to_string())?;
            if w == 0 || h == 0 {
                return Err("empty HEIC image".into());
            }

            // Does this image still need rotating? Compare its aspect ratio with the stored one.
            let o = m.orientation;
            let mut orient = o;
            if o > 1 {
                let stored_landscape = m.size.map(|(dw, dh)| if o >= 5 { dh > dw } else { dw > dh });
                let decoded_landscape = w > h;
                let applied = match stored_landscape {
                    Some(s) if o >= 5 && w != h => {
                        let applied = s != decoded_landscape;
                        APPLIES_ROTATION.store(if applied { 1 } else { 2 }, Ordering::Relaxed);
                        applied
                    }
                    // 180° or mirrored: the aspect doesn't tell, use what we learned (default: applied).
                    _ => APPLIES_ROTATION.load(Ordering::Relaxed) != 2,
                };
                if applied {
                    orient = 1;
                }
            }

            // Scale while still in the decoder's orientation.
            let (bw, bh) = if orient >= 5 { (max_h, max_w) } else { (max_w, max_h) };
            let fit = (bw.min(max_side) as f64 / w as f64).min(bh.min(max_side) as f64 / h as f64).min(1.0);
            let (sw, sh) = (((w as f64 * fit).round() as u32).max(1), ((h as f64 * fit).round() as u32).max(1));
            let scaled: IWICBitmapSource = if (sw, sh) != (w, h) {
                let s = fac.CreateBitmapScaler().map_err(|e| e.to_string())?;
                s.Initialize(&source, sw, sh, WICBitmapInterpolationModeFant).map_err(|e| e.to_string())?;
                s.cast().map_err(|e| e.to_string())?
            } else {
                source
            };
            let conv = fac.CreateFormatConverter().map_err(|e| e.to_string())?;
            conv.Initialize(&scaled, &GUID_WICPixelFormat32bppRGBA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)
                .map_err(|e| e.to_string())?;
            let mut pixels = vec![Color32::BLACK; (sw * sh) as usize];
            conv.CopyPixels(std::ptr::null(), sw * 4, bytemuck::cast_slice_mut(&mut pixels))
                .map_err(|e| format!("{e} ({STORE_HINT}?)"))?;
            // HEIC photos are opaque, so straight RGBA equals premultiplied.
            Ok(crate::decode::orient(pixels, sw as usize, sh as usize, orient))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orientation_from_container() {
        assert_eq!(orientation(0, None), 1);
        assert_eq!(orientation(3, None), 6); // 270° counter-clockwise = 90° clockwise
        assert_eq!(orientation(1, None), 8);
        assert_eq!(orientation(2, None), 3);
        assert_eq!(orientation(0, Some(0)), 2);
    }

    /// Run with: PICKER_HEIC=/path/to/photo.heic cargo test --release heic_file -- --ignored --nocapture
    #[test]
    #[ignore]
    fn heic_file() {
        let Ok(p) = std::env::var("PICKER_HEIC") else { return };
        let m = read_meta(Path::new(&p)).unwrap();
        println!("orientation {} size {:?} date {:?} camera {:?} rating {:?}", m.orientation, m.size, m.date, m.camera, m.rating);
        #[cfg(any(target_os = "macos", windows))]
        for thumb in [true, false] {
            let img = decode(Path::new(&p), &m, thumb, 400, 400, 16384).unwrap();
            println!("decoded (thumb={thumb}): {:?}", img.size);
            // PICKER_HEIC_OUT=dir writes the decoded image as a PPM to look at.
            if let Ok(dir) = std::env::var("PICKER_HEIC_OUT") {
                let mut ppm = format!("P6 {} {} 255\n", img.size[0], img.size[1]).into_bytes();
                ppm.extend(img.pixels.iter().flat_map(|c| [c.r(), c.g(), c.b()]));
                fs::write(Path::new(&dir).join(format!("decoded-{thumb}.ppm")), ppm).unwrap();
            }
        }
    }
}
