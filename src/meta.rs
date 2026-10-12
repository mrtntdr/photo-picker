//! Minimal JPEG / EXIF / XMP handling: reading the few tags we need and
//! writing the star rating back into the file.
//!
//! The rating is written to three places so every common tool sees it:
//!   * EXIF IFD0 `Rating` (0x4746) and `RatingPercent` (0x4749) — Windows Explorer
//!   * XMP `xmp:Rating` — Lightroom, Bridge, Capture One, darktable, digiKam, macOS
//!
//! Writing never moves existing EXIF data (maker notes contain absolute offsets),
//! it only patches values in place or appends a new IFD0 at the end of the TIFF block.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

const EXIF_HEADER: &[u8] = b"Exif\0\0";
const XMP_HEADER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";

const TAG_ORIENTATION: u16 = 0x0112;
const TAG_MAKE: u16 = 0x010F;
const TAG_MODEL: u16 = 0x0110;
const TAG_RATING: u16 = 0x4746;
const TAG_RATING_PERCENT: u16 = 0x4749;
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_EXPOSURE_TIME: u16 = 0x829A;
const TAG_FNUMBER: u16 = 0x829D;
const TAG_ISO: u16 = 0x8827;
const TAG_DATE_ORIGINAL: u16 = 0x9003;
const TAG_FOCAL_LENGTH: u16 = 0x920A;
const TAG_LENS_MODEL: u16 = 0xA434;
const TAG_THUMB_OFFSET: u16 = 0x0201;
const TAG_THUMB_LENGTH: u16 = 0x0202;

#[derive(Clone, Debug, Default)]
pub struct Meta {
    pub orientation: u8,
    pub rating: Option<u8>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub date: Option<String>,
    pub exposure: Option<String>,
    pub aperture: Option<String>,
    pub iso: Option<u32>,
    pub focal: Option<String>,
    /// Pixel dimensions as displayed (after applying orientation).
    pub size: Option<(u32, u32)>,
    /// Embedded EXIF thumbnail (a small JPEG), if present.
    pub thumbnail: Option<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// JPEG segment scanning

struct Segment {
    marker: u8,
    /// Offset of the 0xFF of the marker.
    start: usize,
    /// Offset of the payload (after the 2 length bytes).
    data: usize,
    /// Offset one past the end of the payload.
    end: usize,
}

/// Scans the header segments of a JPEG (everything before the first SOS).
fn scan_segments(buf: &[u8]) -> io::Result<(Vec<Segment>, usize)> {
    if buf.len() < 4 || buf[0] != 0xFF || buf[1] != 0xD8 {
        return Err(invalid("not a JPEG file"));
    }
    let mut segs = Vec::new();
    let mut pos = 2;
    loop {
        // Skip fill bytes.
        while pos < buf.len() && buf[pos] == 0xFF && buf.get(pos + 1) == Some(&0xFF) {
            pos += 1;
        }
        if pos + 4 > buf.len() {
            return Err(invalid("truncated JPEG header"));
        }
        if buf[pos] != 0xFF {
            return Err(invalid("corrupt JPEG marker"));
        }
        let marker = buf[pos + 1];
        if marker == 0xDA || marker == 0xD9 {
            // Start of scan: the rest is image data.
            return Ok((segs, pos));
        }
        let len = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]) as usize;
        if len < 2 || pos + 2 + len > buf.len() {
            return Err(invalid("corrupt JPEG segment length"));
        }
        segs.push(Segment {
            marker,
            start: pos,
            data: pos + 4,
            end: pos + 2 + len,
        });
        pos += 2 + len;
    }
}

/// Reads only the header segments of a file (fast: does not read image data).
/// Returns the payloads of the APP1 segments and the frame size.
fn read_header(path: &Path) -> io::Result<(Vec<Vec<u8>>, Option<(u32, u32)>)> {
    let mut f = io::BufReader::with_capacity(64 * 1024, fs::File::open(path)?);
    let mut two = [0u8; 2];
    f.read_exact(&mut two)?;
    if two != [0xFF, 0xD8] {
        return Err(invalid("not a JPEG file"));
    }
    let mut out = Vec::new();
    let mut dims = None;
    loop {
        let mut b = [0u8; 1];
        f.read_exact(&mut b)?;
        if b[0] != 0xFF {
            return Err(invalid("corrupt JPEG marker"));
        }
        let mut marker = 0xFF;
        while marker == 0xFF {
            f.read_exact(&mut b)?;
            marker = b[0];
        }
        if marker == 0xDA || marker == 0xD9 {
            return Ok((out, dims));
        }
        f.read_exact(&mut two)?;
        let len = u16::from_be_bytes(two) as usize;
        if len < 2 {
            return Err(invalid("corrupt JPEG segment length"));
        }
        if marker == 0xE1 {
            let mut data = vec![0u8; len - 2];
            f.read_exact(&mut data)?;
            out.push(data);
        } else if is_sof(marker) && len >= 7 {
            let mut data = [0u8; 5];
            f.read_exact(&mut data)?;
            dims = sof_dims(&data);
            f.seek_relative((len - 7) as i64)?;
        } else {
            f.seek_relative((len - 2) as i64)?;
        }
    }
}

fn is_sof(marker: u8) -> bool {
    matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC)
}

/// Width/height from the start of a SOF segment payload.
fn sof_dims(d: &[u8]) -> Option<(u32, u32)> {
    let h = u16::from_be_bytes([*d.get(1)?, *d.get(2)?]) as u32;
    let w = u16::from_be_bytes([*d.get(3)?, *d.get(4)?]) as u32;
    (w > 0 && h > 0).then_some((w, h))
}

fn with_dims(mut meta: Meta, dims: Option<(u32, u32)>) -> Meta {
    meta.size = dims.map(|(w, h)| if meta.orientation >= 5 { (h, w) } else { (w, h) });
    meta
}

pub fn read_meta_from_file(path: &Path) -> io::Result<Meta> {
    let (app1, dims) = read_header(path)?;
    Ok(with_dims(parse_app1s(app1.iter().map(|v| v.as_slice())), dims))
}

/// Parses metadata from a complete in-memory JPEG.
pub fn read_meta_from_bytes(buf: &[u8]) -> Meta {
    match scan_segments(buf) {
        Ok((segs, _)) => {
            let dims = segs
                .iter()
                .find(|s| is_sof(s.marker))
                .and_then(|s| sof_dims(&buf[s.data..s.end]));
            let app1s = segs.iter().filter(|s| s.marker == 0xE1).map(|s| &buf[s.data..s.end]);
            with_dims(parse_app1s(app1s), dims)
        }
        Err(_) => Meta {
            orientation: 1,
            ..Default::default()
        },
    }
}

fn parse_app1s<'a>(app1s: impl Iterator<Item = &'a [u8]>) -> Meta {
    let mut meta = Meta {
        orientation: 1,
        ..Default::default()
    };
    let mut exif_rating = None;
    let mut xmp_rating = None;
    for data in app1s {
        if let Some(tiff) = data.strip_prefix(EXIF_HEADER) {
            if let Some(t) = Tiff::new(tiff) {
                exif_rating = parse_exif(&t, &mut meta);
            }
        } else if let Some(xmp) = data.strip_prefix(XMP_HEADER) {
            xmp_rating = xmp_find_rating(&String::from_utf8_lossy(xmp));
        }
    }
    // XMP is what Lightroom & co. treat as authoritative.
    meta.rating = xmp_rating.or(exif_rating);
    meta
}

// ---------------------------------------------------------------------------
// TIFF / IFD parsing

struct Tiff<'a> {
    d: &'a [u8],
    le: bool,
}

#[derive(Clone, Copy)]
struct Entry {
    tag: u16,
    typ: u16,
    count: u32,
    /// Offset (within the TIFF block) of the 4-byte value/offset field.
    field: usize,
}

fn type_size(typ: u16) -> usize {
    match typ {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => 1,
    }
}

impl<'a> Tiff<'a> {
    fn new(d: &'a [u8]) -> Option<Self> {
        let le = match d.get(0..4)? {
            b"II*\0" => true,
            b"MM\0*" => false,
            _ => return None,
        };
        Some(Tiff { d, le })
    }

    fn u16(&self, off: usize) -> Option<u16> {
        let b: [u8; 2] = self.d.get(off..off + 2)?.try_into().ok()?;
        Some(if self.le { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    }

    fn u32(&self, off: usize) -> Option<u32> {
        let b: [u8; 4] = self.d.get(off..off + 4)?.try_into().ok()?;
        Some(if self.le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    fn ifd0_offset(&self) -> Option<usize> {
        self.u32(4).map(|v| v as usize)
    }

    /// Returns the entries and the offset of the "next IFD" field.
    fn ifd(&self, off: usize) -> Option<(Vec<Entry>, usize)> {
        let n = self.u16(off)? as usize;
        let mut v = Vec::with_capacity(n);
        for i in 0..n {
            let e = off + 2 + i * 12;
            v.push(Entry {
                tag: self.u16(e)?,
                typ: self.u16(e + 2)?,
                count: self.u32(e + 4)?,
                field: e + 8,
            });
        }
        let next = off + 2 + n * 12;
        self.d.get(next..next + 4)?;
        Some((v, next))
    }

    fn value_bytes(&self, e: &Entry) -> Option<&'a [u8]> {
        let size = type_size(e.typ).checked_mul(e.count as usize)?;
        let off = if size <= 4 { e.field } else { self.u32(e.field)? as usize };
        self.d.get(off..off.checked_add(size)?)
    }

    fn uint(&self, e: &Entry) -> Option<u32> {
        match e.typ {
            1 => self.d.get(e.field).map(|&b| b as u32),
            3 => self.u16(e.field).map(|v| v as u32),
            4 => self.u32(e.field),
            _ => None,
        }
    }

    fn rational(&self, e: &Entry) -> Option<(u32, u32)> {
        if e.typ != 5 && e.typ != 10 {
            return None;
        }
        let off = self.u32(e.field)? as usize;
        Some((self.u32(off)?, self.u32(off + 4)?))
    }

    fn string(&self, e: &Entry) -> Option<String> {
        if e.typ != 2 {
            return None;
        }
        let b = self.value_bytes(e)?;
        let b = b.split(|&c| c == 0).next().unwrap_or(b);
        let s = String::from_utf8_lossy(b).trim().to_string();
        (!s.is_empty()).then_some(s)
    }
}

fn parse_exif(t: &Tiff, meta: &mut Meta) -> Option<u8> {
    let (ifd0, next) = t.ifd(t.ifd0_offset()?)?;
    let mut rating = None;
    let mut make = None;
    let mut model = None;
    let mut exif_ifd = None;
    for e in &ifd0 {
        match e.tag {
            TAG_ORIENTATION => meta.orientation = t.uint(e).unwrap_or(1).clamp(1, 8) as u8,
            TAG_RATING => rating = t.uint(e).map(|v| v.min(5) as u8),
            TAG_MAKE => make = t.string(e),
            TAG_MODEL => model = t.string(e),
            TAG_EXIF_IFD => exif_ifd = t.uint(e),
            _ => {}
        }
    }
    meta.camera = match (make, model) {
        (Some(ma), Some(mo)) if mo.starts_with(&ma) => Some(mo),
        (Some(ma), Some(mo)) => Some(format!("{ma} {mo}")),
        (a, b) => a.or(b),
    };

    if let Some((exif, _)) = exif_ifd.and_then(|o| t.ifd(o as usize)) {
        for e in &exif {
            match e.tag {
                TAG_EXPOSURE_TIME => {
                    meta.exposure = t.rational(e).filter(|r| r.1 != 0).map(|(n, d)| {
                        let s = n as f64 / d as f64;
                        if s >= 0.3 {
                            format!("{}s", trim_float(s))
                        } else {
                            format!("1/{}s", (1.0 / s).round())
                        }
                    })
                }
                TAG_FNUMBER => {
                    meta.aperture = t
                        .rational(e)
                        .filter(|r| r.1 != 0)
                        .map(|(n, d)| format!("f/{}", trim_float(n as f64 / d as f64)))
                }
                TAG_ISO => meta.iso = t.uint(e),
                TAG_DATE_ORIGINAL => meta.date = t.string(e),
                TAG_FOCAL_LENGTH => {
                    meta.focal = t
                        .rational(e)
                        .filter(|r| r.1 != 0)
                        .map(|(n, d)| format!("{}mm", trim_float(n as f64 / d as f64)))
                }
                TAG_LENS_MODEL => meta.lens = t.string(e),
                _ => {}
            }
        }
    }

    // IFD1 holds the embedded thumbnail.
    let ifd1 = t.u32(next).unwrap_or(0) as usize;
    if ifd1 != 0 {
        if let Some((entries, _)) = t.ifd(ifd1) {
            let find = |tag| entries.iter().find(|e| e.tag == tag).and_then(|e| t.uint(e));
            if let (Some(off), Some(len)) = (find(TAG_THUMB_OFFSET), find(TAG_THUMB_LENGTH)) {
                let (off, len) = (off as usize, len as usize);
                if let Some(b) = t.d.get(off..off + len) {
                    meta.thumbnail = Some(b.to_vec());
                }
            }
        }
    }
    rating
}

fn trim_float(v: f64) -> String {
    let s = format!("{v:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

// ---------------------------------------------------------------------------
// XMP

fn xmp_find_rating(xmp: &str) -> Option<u8> {
    let (start, end) = xmp_value_span(xmp, "xmp:Rating")?;
    xmp[start..end].trim().parse::<i32>().ok().map(|v| v.clamp(0, 5) as u8)
}

/// Byte range of a property value inside the XMP text, either in attribute
/// form (`xmp:Rating="4"`) or element form (`<xmp:Rating>4</xmp:Rating>`).
fn xmp_value_span(xmp: &str, name: &str) -> Option<(usize, usize)> {
    for quote in ['"', '\''] {
        let pat = format!("{name}={quote}");
        if let Some(i) = xmp.find(&pat) {
            let start = i + pat.len();
            let end = start + xmp[start..].find(quote)?;
            return Some((start, end));
        }
    }
    let open = format!("<{name}>");
    let i = xmp.find(&open)?;
    let start = i + open.len();
    let end = start + xmp[start..].find(&format!("</{name}>"))?;
    Some((start, end))
}

fn xmp_replace(xmp: &str, name: &str, value: impl std::fmt::Display) -> Option<String> {
    let (start, end) = xmp_value_span(xmp, name)?;
    Some(format!("{}{}{}", &xmp[..start], value, &xmp[end..]))
}

fn xmp_with_rating(xmp: Option<&str>, rating: u8) -> String {
    if let Some(xmp) = xmp {
        // Windows also keeps a percentage copy; keep it consistent if present.
        let owned;
        let xmp = match xmp_replace(xmp, "MicrosoftPhoto:Rating", rating_percent(rating)) {
            Some(x) => {
                owned = x;
                owned.as_str()
            }
            None => xmp,
        };
        if let Some(x) = xmp_replace(xmp, "xmp:Rating", rating) {
            return x;
        }
        // Add a separate rdf:Description right after the <rdf:RDF ...> tag, so we
        // don't depend on which namespaces the existing descriptions declare.
        if let Some(i) = xmp.find("<rdf:RDF") {
            if let Some(gt) = xmp[i..].find('>') {
                let at = i + gt + 1;
                return format!(
                    "{}\n  <rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmp:Rating=\"{}\"/>{}",
                    &xmp[..at],
                    rating,
                    &xmp[at..]
                );
            }
        }
    }
    format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n \
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  \
         <rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmp:Rating=\"{rating}\"/>\n \
         </rdf:RDF>\n\
         </x:xmpmeta>\n\
         <?xpacket end=\"w\"?>"
    )
}

// ---------------------------------------------------------------------------
// EXIF writing

/// Windows' mapping of stars to RatingPercent.
fn rating_percent(r: u8) -> u16 {
    match r {
        0 => 0,
        1 => 1,
        2 => 25,
        3 => 50,
        4 => 75,
        _ => 99,
    }
}

struct TiffW {
    d: Vec<u8>,
    le: bool,
}

impl TiffW {
    fn put_u16(&mut self, off: usize, v: u16) {
        let b = if self.le { v.to_le_bytes() } else { v.to_be_bytes() };
        self.d[off..off + 2].copy_from_slice(&b);
    }
    fn put_u32(&mut self, off: usize, v: u32) {
        let b = if self.le { v.to_le_bytes() } else { v.to_be_bytes() };
        self.d[off..off + 4].copy_from_slice(&b);
    }
    fn push_u16(&mut self, v: u16) {
        let b = if self.le { v.to_le_bytes() } else { v.to_be_bytes() };
        self.d.extend_from_slice(&b);
    }
    fn push_u32(&mut self, v: u32) {
        let b = if self.le { v.to_le_bytes() } else { v.to_be_bytes() };
        self.d.extend_from_slice(&b);
    }
}

/// Returns a new TIFF block with the rating tags set in IFD0.
fn tiff_with_rating(tiff: &[u8], rating: u8) -> io::Result<Vec<u8>> {
    let wanted = [(TAG_RATING, rating as u16), (TAG_RATING_PERCENT, rating_percent(rating))];
    let t = Tiff::new(tiff).ok_or_else(|| invalid("bad TIFF header in EXIF"))?;
    let ifd0_off = t.ifd0_offset().ok_or_else(|| invalid("bad EXIF"))?;
    let (entries, next_field) = t.ifd(ifd0_off).ok_or_else(|| invalid("bad EXIF IFD0"))?;
    let next_ifd = t.u32(next_field).unwrap_or(0);
    let le = t.le;

    let mut w = TiffW { d: tiff.to_vec(), le };

    // Patch in place where the tag already exists with a usable type.
    let mut missing = Vec::new();
    for (tag, val) in wanted {
        match entries.iter().find(|e| e.tag == tag) {
            Some(e) if e.typ == 3 && e.count == 1 => w.put_u16(e.field, val),
            Some(e) if e.typ == 4 && e.count == 1 => w.put_u32(e.field, val as u32),
            _ => missing.push((tag, val)),
        }
    }
    if missing.is_empty() {
        return Ok(w.d);
    }

    // Append a new IFD0 at the end: all old entries (their values/offsets stay
    // valid because nothing moves) plus the new/replaced ones.
    let mut new_entries: Vec<[u8; 12]> = entries
        .iter()
        .filter(|e| !missing.iter().any(|(tag, _)| *tag == e.tag))
        .map(|e| {
            let start = e.field - 8;
            tiff[start..start + 12].try_into().unwrap()
        })
        .collect();
    for (tag, val) in &missing {
        let mut tmp = TiffW { d: vec![0; 12], le };
        tmp.put_u16(0, *tag);
        tmp.put_u16(2, 3); // SHORT
        tmp.put_u32(4, 1);
        tmp.put_u16(8, *val);
        new_entries.push(tmp.d.try_into().unwrap());
    }
    let tag_of = |e: &[u8; 12]| {
        if le { u16::from_le_bytes([e[0], e[1]]) } else { u16::from_be_bytes([e[0], e[1]]) }
    };
    new_entries.sort_by_key(tag_of);

    if w.d.len() % 2 == 1 {
        w.d.push(0);
    }
    let new_off = w.d.len();
    w.push_u16(new_entries.len() as u16);
    for e in &new_entries {
        w.d.extend_from_slice(e);
    }
    w.push_u32(next_ifd);
    w.put_u32(4, new_off as u32);
    Ok(w.d)
}

fn new_tiff_with_rating(rating: u8) -> Vec<u8> {
    let mut w = TiffW { d: b"MM\0*\0\0\0\x08".to_vec(), le: false };
    w.push_u16(2);
    for (tag, val) in [(TAG_RATING, rating as u16), (TAG_RATING_PERCENT, rating_percent(rating))] {
        w.push_u16(tag);
        w.push_u16(3);
        w.push_u32(1);
        w.push_u16(val);
        w.push_u16(0);
    }
    w.push_u32(0);
    w.d
}

fn push_segment(out: &mut Vec<u8>, marker: u8, header: &[u8], body: &[u8]) -> io::Result<()> {
    let len = 2 + header.len() + body.len();
    if len > 0xFFFF {
        return Err(invalid("metadata segment would exceed 64 KB"));
    }
    out.extend_from_slice(&[0xFF, marker]);
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(header);
    out.extend_from_slice(body);
    Ok(())
}

/// Returns a copy of the JPEG with the rating set in EXIF and XMP. If the EXIF
/// segment has no room left (e.g. Fujifilm fills it to the 64 KB limit), the
/// EXIF is kept as is and the rating goes into XMP only, which is what
/// Lightroom, Bridge, Photos and Windows Explorer read.
pub fn jpeg_with_rating(buf: &[u8], rating: u8) -> io::Result<Vec<u8>> {
    let rating = rating.min(5);
    let (segs, sos) = scan_segments(buf)?;
    let is_exif = |s: &Segment| s.marker == 0xE1 && buf[s.data..s.end].starts_with(EXIF_HEADER);
    let is_xmp = |s: &Segment| s.marker == 0xE1 && buf[s.data..s.end].starts_with(XMP_HEADER);

    let exif_tiff = match segs.iter().find(|s| is_exif(s)) {
        Some(s) => {
            let old = &buf[s.data + EXIF_HEADER.len()..s.end];
            let new = tiff_with_rating(old, rating)?;
            if 2 + EXIF_HEADER.len() + new.len() > 0xFFFF { old.to_vec() } else { new }
        }
        None => new_tiff_with_rating(rating),
    };
    let old_xmp = segs
        .iter()
        .find(|s| is_xmp(s))
        .map(|s| String::from_utf8_lossy(&buf[s.data + XMP_HEADER.len()..s.end]).into_owned());
    let xmp = xmp_with_rating(old_xmp.as_deref(), rating);

    let mut out = Vec::with_capacity(buf.len() + 4096);
    out.extend_from_slice(&[0xFF, 0xD8]);

    let has_exif = segs.iter().any(|s| is_exif(s));
    let has_xmp = segs.iter().any(|s| is_xmp(s));
    // New EXIF goes after JFIF APP0 (if first) otherwise directly after SOI.
    let insert_after_app0 = segs.first().is_some_and(|s| s.marker == 0xE0);
    let mut exif_written = false;
    let mut xmp_written = false;

    let write_new = |out: &mut Vec<u8>, exif_written: &mut bool, xmp_written: &mut bool| -> io::Result<()> {
        if !has_exif && !*exif_written {
            push_segment(out, 0xE1, EXIF_HEADER, &exif_tiff)?;
            *exif_written = true;
        }
        if !has_xmp && !*xmp_written {
            push_segment(out, 0xE1, XMP_HEADER, xmp.as_bytes())?;
            *xmp_written = true;
        }
        Ok(())
    };

    if !insert_after_app0 {
        write_new(&mut out, &mut exif_written, &mut xmp_written)?;
    }
    for (i, s) in segs.iter().enumerate() {
        if is_exif(s) && !exif_written {
            push_segment(&mut out, 0xE1, EXIF_HEADER, &exif_tiff)?;
            exif_written = true;
        } else if is_exif(s) {
            // Duplicate EXIF segment: drop it.
        } else if is_xmp(s) && !xmp_written {
            push_segment(&mut out, 0xE1, XMP_HEADER, xmp.as_bytes())?;
            xmp_written = true;
        } else if is_xmp(s) {
        } else {
            out.extend_from_slice(&buf[s.start..s.end]);
        }
        if i == 0 && insert_after_app0 {
            write_new(&mut out, &mut exif_written, &mut xmp_written)?;
        }
    }
    out.extend_from_slice(&buf[sos..]);
    Ok(out)
}

/// Writes the rating into the file on disk, atomically (temp file + rename),
/// keeping the file's modification time (and, on macOS, its extended
/// attributes, with a "Picked" Finder tag on picked photos).
pub fn write_rating(path: &Path, rating: u8) -> io::Result<()> {
    let buf = fs::read(path)?;
    let new = jpeg_with_rating(&buf, rating)?;

    // Sanity check before touching the original.
    let check = read_meta_from_bytes(&new);
    if check.rating != Some(rating.min(5)) {
        return Err(invalid("rating verification failed; file left unchanged"));
    }

    let md = fs::metadata(path)?;
    let mtime = filetime::FileTime::from_last_modification_time(&md);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("photo");
    let tmp = path.with_file_name(format!(".{name}.picker-tmp"));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&new)?;
        f.sync_all()?;
    }
    #[cfg(target_os = "macos")]
    if let Err(e) = crate::mac_tags::carry_over(path, &tmp, rating >= crate::app::PICKED_MIN) {
        let _ = fs::remove_file(&tmp);
        return Err(io::Error::new(e.kind(), format!("Finder tags: {e}")));
    }
    let _ = filetime::set_file_mtime(&tmp, mtime);
    // On Windows, antivirus or Explorer's thumbnailer can briefly lock the file.
    let mut attempt = 0;
    loop {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) if attempt < 5 && e.kind() == io::ErrorKind::PermissionDenied && cfg!(windows) => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(100 * attempt));
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        }
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_jpeg(app: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        for (m, body) in app {
            push_segment(&mut v, *m, &[], body).unwrap();
        }
        v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 1, 2, 3, 0xFF, 0xD9]);
        v
    }

    #[test]
    fn adds_rating_to_file_without_metadata() {
        let jpg = minimal_jpeg(&[(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0".to_vec())]);
        let out = jpeg_with_rating(&jpg, 4).unwrap();
        assert_eq!(read_meta_from_bytes(&out).rating, Some(4));
        assert!(out.ends_with(&[0xFF, 0xDA, 0x00, 0x02, 1, 2, 3, 0xFF, 0xD9]));
        let again = jpeg_with_rating(&out, 1).unwrap();
        assert_eq!(read_meta_from_bytes(&again).rating, Some(1));
        assert_eq!(again.len(), out.len());
    }

    #[test]
    fn keeps_existing_exif_entries() {
        // Little-endian TIFF with IFD0 = { Orientation = 6 }.
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&TAG_ORIENTATION.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&[6, 0, 0, 0]);
        tiff.extend_from_slice(&0u32.to_le_bytes());
        let mut exif = EXIF_HEADER.to_vec();
        exif.extend_from_slice(&tiff);
        let jpg = minimal_jpeg(&[(0xE1, exif)]);

        let out = jpeg_with_rating(&jpg, 4).unwrap();
        let m = read_meta_from_bytes(&out);
        assert_eq!(m.rating, Some(4));
        assert_eq!(m.orientation, 6);

        let out2 = jpeg_with_rating(&out, 2).unwrap();
        assert_eq!(read_meta_from_bytes(&out2).rating, Some(2));
        assert_eq!(out2.len(), out.len(), "second write should patch in place");
    }

    #[test]
    fn full_exif_falls_back_to_xmp_only() {
        // An EXIF segment with no room for the Rating entries.
        // Little-endian TIFF with an empty IFD0, padded to the size limit.
        let mut tiff = b"II*\0\x08\0\0\0\0\0\0\0\0\0".to_vec();
        tiff.resize(0xFFFF - 2 - EXIF_HEADER.len(), 0);
        let mut exif = EXIF_HEADER.to_vec();
        exif.extend_from_slice(&tiff);
        let mut xmp = XMP_HEADER.to_vec();
        xmp.extend_from_slice(xmp_with_rating(None, 0).as_bytes());
        let jpeg = minimal_jpeg(&[(0xE1, exif.clone()), (0xE1, xmp)]);

        let out = jpeg_with_rating(&jpeg, 4).unwrap();
        assert_eq!(read_meta_from_bytes(&out).rating, Some(4));
        let (segs, _) = scan_segments(&out).unwrap();
        assert_eq!(&out[segs[0].data..segs[0].end], &exif[..], "EXIF must be left untouched");
    }

    #[test]
    fn updates_existing_xmp() {
        let xmp = xmp_with_rating(None, 3);
        let x2 = xmp_with_rating(Some(&xmp), 5);
        assert_eq!(xmp_find_rating(&x2), Some(5));
        let elem = "<x:xmpmeta><rdf:RDF><rdf:Description><xmp:Rating>2</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
        assert_eq!(xmp_find_rating(&xmp_with_rating(Some(elem), 4)), Some(4));
        let none = "<x:xmpmeta><rdf:RDF xmlns:rdf=\"r\"><rdf:Description rdf:about=\"\"/></rdf:RDF></x:xmpmeta>";
        assert_eq!(xmp_find_rating(&xmp_with_rating(Some(none), 4)), Some(4));
    }
}

/// Run with: PICKER_TEST_DIR=/path/to/copies cargo test --release -- --ignored --nocapture
/// Rewrites ratings in the given folder (use copies!).
#[cfg(test)]
#[test]
#[ignore]
fn real_files_roundtrip() {
    let Ok(dir) = std::env::var("PICKER_TEST_DIR") else { return };
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if !crate::workers::is_jpeg(&path) {
            continue;
        }
        let orig = fs::read(&path).unwrap();
        let (_, sos) = scan_segments(&orig).unwrap();
        let before = read_meta_from_bytes(&orig);
        for r in [1, 4, 0, 4] {
            let t = std::time::Instant::now();
            write_rating(&path, r).unwrap();
            let el = t.elapsed();
            let m = read_meta_from_file(&path).unwrap();
            assert_eq!(m.rating, Some(r));
            assert_eq!(m.orientation, before.orientation);
            assert_eq!(m.camera, before.camera);
            assert_eq!(m.size, before.size);
            let now = fs::read(&path).unwrap();
            let (_, sos2) = scan_segments(&now).unwrap();
            assert_eq!(&orig[sos..], &now[sos2..], "image data must be untouched");
            if r == 1 {
                println!("{:?}: write {:?}, +{} bytes", path.file_name().unwrap(), el, now.len() as i64 - orig.len() as i64);
            }
        }
        let data = fs::read(&path).unwrap();
        for (w, h) in [(3072, 1728), (1920, 1080), (256, 256), (u32::MAX, u32::MAX)] {
            let t = std::time::Instant::now();
            let img = crate::decode::decode_jpeg(&data, before.orientation, w, h, 16384).unwrap();
            println!("   decode for {w}x{h}: {:?} -> {:?}", t.elapsed(), img.size);
        }
        let t = std::time::Instant::now();
        let img = crate::decode::decode_jpeg_full(&data, before.orientation, 16384).unwrap();
        println!("   full (zune): {:?} -> {:?}", t.elapsed(), img.size);
        assert_eq!(Some((img.size[0] as u32, img.size[1] as u32)), before.size);
    }
}
