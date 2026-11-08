//! Keywords ("tags"): reading them and adding or removing one, in JPEGs only.
//!
//! A keyword is written to two places, which Synology Photos, Lightroom & co. read:
//!   * XMP `dc:subject` (wins when both exist)
//!   * IPTC `Keywords` (dataset 2:25) in the Photoshop APP13 segment, marked as UTF-8 (1:90)
//!
//! Edits are surgical: only the `rdf:li` entries of `dc:subject` and the IPTC datasets
//! change, everything else in those segments is kept byte for byte, and all other
//! segments and the image data are copied. Each result is checked by [`verify`] before
//! it can reach the disk. Anything we don't fully understand is refused, not guessed at.

use std::fs;
use std::io;
use std::ops::Range;
use std::path::Path;

use quick_xml::NsReader;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{Namespace, ResolveResult};
use unicode_normalization::UnicodeNormalization;

use crate::meta::{self, Segment, XMP_HEADER, invalid, push_segment, scan_segments};

/// The IPTC spec's limit for one keyword, in bytes (õ, ä, ö, ü take two).
pub const MAX_KEYWORD_BYTES: usize = 64;

const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const DC_NS: &str = "http://purl.org/dc/elements/1.1/";
const EMPTY_XMP: &str = "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
     <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n \
     <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n \
     </rdf:RDF>\n\
     </x:xmpmeta>\n\
     <?xpacket end=\"w\"?>";

const PHOTOSHOP_HEADER: &[u8] = b"Photoshop 3.0\0";
const MARKER_APP13: u8 = 0xED;
const MAX_APP13_PAYLOAD: usize = 0xFFFF - 2 - PHOTOSHOP_HEADER.len();
const IRB_SIGNATURES: [&[u8]; 5] = [b"8BIM", b"PHUT", b"DCSR", b"AgHg", b"MeSa"];
const IRB_IPTC: u16 = 0x0404;
const IRB_IPTC_DIGEST: u16 = 0x0425;

const IPTC_CHARSET: (u8, u8) = (1, 90);
const IPTC_RECORD_VERSION: (u8, u8) = (2, 0);
const IPTC_KEYWORDS: (u8, u8) = (2, 25);
const ESC_UTF8: &[u8] = b"\x1B%G";
/// IIM version 4, what every current writer uses.
const RECORD_VERSION_4: [u8; 2] = [0, 4];
/// Record 2 datasets holding text, the ones a change of character set affects.
const IPTC_TEXT: [u8; 25] = [
    5, 7, 12, 15, 20, 22, 25, 26, 40, 65, 80, 85, 90, 92, 95, 100, 101, 103, 105, 110, 115, 116, 118, 120, 122,
];

/// Keywords differing only in case or Unicode form are the same keyword.
pub fn same_keyword(a: &str, b: &str) -> bool {
    fold(a) == fold(b)
}

pub fn contains(keywords: &[String], keyword: &str) -> bool {
    keywords.iter().any(|k| same_keyword(k, keyword))
}

fn fold(s: &str) -> String {
    s.trim().nfc().collect::<String>().to_lowercase()
}

// ---------------------------------------------------------------------------
// XMP

#[derive(Default)]
struct Subject {
    /// The whole `dc:subject` element.
    element: Range<usize>,
    items: Vec<Item>,
    /// Where entries go in a `rdf:Bag` that has none.
    bag_end: usize,
    /// The span of a self-closed `<rdf:Bag/>`.
    empty_bag: Option<Range<usize>>,
    /// As written in this packet, e.g. `rdf:Bag`.
    bag_name: String,
    li_name: String,
}

struct Item {
    span: Range<usize>,
    text: String,
}

#[derive(Default)]
struct XmpScan {
    subject: Option<Subject>,
    /// Just after the `<rdf:RDF ...>` tag.
    rdf_body: Option<usize>,
    rdf_prefix: String,
    /// `rdf:about` of the first description, which any new one must repeat.
    about: Option<String>,
}

#[derive(PartialEq)]
enum Place {
    Outside,
    InSubject,
    InBag,
    InItem,
}

fn scan_xmp(xmp: &str) -> io::Result<XmpScan> {
    let mut r = NsReader::from_str(xmp);
    let mut scan = XmpScan::default();
    let mut sub = Subject::default();
    let mut place = Place::Outside;
    let mut item_start = 0;
    let mut text = String::new();
    loop {
        let start = r.buffer_position() as usize;
        let event = r.read_event().map_err(|e| invalid(&format!("cannot parse XMP: {e}")))?;
        let end = r.buffer_position() as usize;
        let is = |uri: &str, name: &str, e: &BytesStart| {
            let (ns, local) = r.resolver().resolve_element(e.name());
            matches!(ns, ResolveResult::Bound(Namespace(u)) if u == uri) && local.as_ref() == name
        };
        match event {
            Event::Start(e) | Event::Empty(e) if place == Place::Outside => {
                let closed = xmp.as_bytes()[end - 2] == b'/';
                if is(RDF_NS, "RDF", &e) && !closed {
                    scan.rdf_body = Some(end);
                    scan.rdf_prefix = prefix_of(&e);
                } else if is(RDF_NS, "Description", &e) {
                    check_description(&r, &e, &mut scan)?;
                } else if is(DC_NS, "subject", &e) {
                    if closed {
                        return Err(invalid("unsupported dc:subject form in XMP"));
                    }
                    if scan.subject.is_some() {
                        return Err(invalid("more than one dc:subject in XMP"));
                    }
                    sub = Subject { element: start..start, ..Default::default() };
                    place = Place::InSubject;
                }
                // Other elements' children are found by the same walk: no nesting is tracked
                // because the matches above are namespace-qualified.
            }
            Event::Start(e) | Event::Empty(e) if place == Place::InSubject => {
                let closed = xmp.as_bytes()[end - 2] == b'/';
                if !is(RDF_NS, "Bag", &e) && !is(RDF_NS, "Seq", &e) {
                    return Err(invalid("unsupported dc:subject form in XMP"));
                }
                sub.bag_name = e.name().as_ref().to_string();
                let prefix = prefix_of(&e);
                sub.li_name = if prefix.is_empty() { "li".into() } else { format!("{prefix}:li") };
                if closed {
                    sub.empty_bag = Some(start..end);
                } else {
                    place = Place::InBag;
                }
            }
            Event::Start(e) if place == Place::InBag && is(RDF_NS, "li", &e) => {
                item_start = start;
                text.clear();
                place = Place::InItem;
            }
            Event::Empty(e) if place == Place::InBag && is(RDF_NS, "li", &e) => {
                sub.items.push(Item { span: start..end, text: String::new() });
            }
            Event::Start(_) | Event::Empty(_) if place == Place::InBag || place == Place::InItem => {
                return Err(invalid("unsupported dc:subject form in XMP"));
            }
            Event::Text(e) if place == Place::InItem => text.push_str(&e.xml10_content()),
            Event::CData(e) if place == Place::InItem => text.push_str(&e.xml10_content()),
            Event::GeneralRef(e) if place == Place::InItem => {
                if let Some(c) = e.resolve_char_ref().map_err(|e| invalid(&format!("cannot parse XMP: {e}")))? {
                    text.push(c);
                } else {
                    let name = e.xml10_content();
                    let value = quick_xml::escape::resolve_xml_entity(&name).ok_or_else(|| invalid("unknown entity in XMP"))?;
                    text.push_str(value);
                }
            }
            Event::Text(e) if place == Place::InSubject && !e.xml10_content().trim().is_empty() => {
                return Err(invalid("unsupported dc:subject form in XMP"));
            }
            Event::End(_) if place == Place::InItem => {
                sub.items.push(Item { span: item_start..end, text: std::mem::take(&mut text) });
                place = Place::InBag;
            }
            Event::End(_) if place == Place::InBag => {
                sub.bag_end = start;
                place = Place::InSubject;
            }
            Event::End(_) if place == Place::InSubject => {
                if sub.bag_name.is_empty() {
                    return Err(invalid("unsupported dc:subject form in XMP"));
                }
                sub.element.end = end;
                scan.subject = Some(std::mem::take(&mut sub));
                place = Place::Outside;
            }
            Event::Eof if place == Place::Outside => return Ok(scan),
            Event::Eof => return Err(invalid("cannot parse XMP: unexpected end")),
            _ => {}
        }
    }
}

fn prefix_of(e: &BytesStart) -> String {
    e.name().prefix().map_or(String::new(), |p| p.as_ref().to_string())
}

fn check_description(r: &NsReader<&[u8]>, e: &BytesStart, scan: &mut XmpScan) -> io::Result<()> {
    for attr in e.attributes().flatten() {
        let (ns, local) = r.resolver().resolve_attribute(attr.key);
        let in_ns = |uri: &str| matches!(ns, ResolveResult::Bound(Namespace(u)) if u == uri);
        if in_ns(DC_NS) && local.as_ref() == "subject" {
            return Err(invalid("unsupported dc:subject form in XMP"));
        }
        if in_ns(RDF_NS) && local.as_ref() == "about" && scan.about.is_none() {
            scan.about = Some(attr.value.to_string());
        }
    }
    Ok(())
}

/// `dc:subject`, or `None` if there is none or the XMP can't be read.
pub fn read_xmp(xmp: &str) -> Option<Vec<String>> {
    let subject = scan_xmp(xmp).ok()?.subject?;
    Some(subject.items.into_iter().map(|i| i.text).collect())
}

/// Start of the whitespace before `at`, so entries are removed with their line.
fn ws_start(xmp: &str, at: usize) -> usize {
    xmp[..at].trim_end().len()
}

/// Returns the new packet, or `None` if nothing has to change. `seed` is what to
/// start from when there is no `dc:subject` yet (so existing IPTC keywords, hidden
/// once XMP exists, come along).
fn edit_xmp(xmp: &str, keyword: &str, on: bool, seed: &[String]) -> io::Result<Option<String>> {
    let scan = scan_xmp(xmp)?;
    let esc = |k: &str| quick_xml::escape::escape(k.to_string()).into_owned();
    let Some(sub) = &scan.subject else {
        if !on {
            return Ok(None);
        }
        let at = scan.rdf_body.ok_or_else(|| invalid("no rdf:RDF in XMP"))?;
        if scan.rdf_prefix.is_empty() {
            return Err(invalid("unsupported RDF prefix in XMP"));
        }
        let p = &scan.rdf_prefix;
        let mut lis: Vec<&str> = seed.iter().map(String::as_str).collect();
        if !contains(seed, keyword) {
            lis.push(keyword);
        }
        let lis: String = lis.iter().map(|k| format!("<{p}:li>{}</{p}:li>", esc(k))).collect();
        let about = scan.about.as_deref().unwrap_or("");
                return Ok(Some(format!(
            "{}\n  <{p}:Description {p}:about=\"{about}\" xmlns:dc=\"{DC_NS}\"><dc:subject><{p}:Bag>{lis}</{p}:Bag></dc:subject></{p}:Description>{}",
            &xmp[..at],
            &xmp[at..]
        )));
    };

    let matching = sub.items.iter().filter(|i| same_keyword(&i.text, keyword)).count();
    if on {
        if matching > 0 {
            return Ok(None);
        }
        let li = format!("<{0}>{1}</{0}>", sub.li_name, esc(keyword));
        return Ok(Some(if let Some(last) = sub.items.last() {
            // Same indentation as the entry before it.
            let ws = &xmp[ws_start(xmp, last.span.start)..last.span.start];
            format!("{}{ws}{li}{}", &xmp[..last.span.end], &xmp[last.span.end..])
        } else if let Some(bag) = &sub.empty_bag {
            format!("{}<{name}>{li}</{name}>{}", &xmp[..bag.start], &xmp[bag.end..], name = sub.bag_name)
        } else {
            format!("{}{li}{}", &xmp[..sub.bag_end], &xmp[sub.bag_end..])
        }));
    }
    if matching == 0 {
        return Ok(None);
    }
    if matching == sub.items.len() {
        let e = &sub.element;
        return Ok(Some(format!("{}{}", &xmp[..ws_start(xmp, e.start)], &xmp[e.end..])));
    }
    let mut out = xmp.to_string();
    for item in sub.items.iter().rev().filter(|i| same_keyword(&i.text, keyword)) {
        out.replace_range(ws_start(xmp, item.span.start)..item.span.end, "");
    }
    Ok(Some(out))
}

// ---------------------------------------------------------------------------
// Photoshop image resource blocks (the content of APP13)

struct Irb {
    id: u16,
    /// The whole block, padding included.
    span: Range<usize>,
    data: Range<usize>,
}

fn parse_irbs(s: &[u8]) -> Option<Vec<Irb>> {
    let mut out = Vec::new();
    let mut pos = 0;
    // Some writers pad the end with zeros.
    while s[pos..].iter().any(|&b| b != 0) {
        if !IRB_SIGNATURES.contains(&s.get(pos..pos + 4)?) {
            return None;
        }
        let id = u16::from_be_bytes(s.get(pos + 4..pos + 6)?.try_into().ok()?);
        // The name is a Pascal string padded to an even length.
        let name_len = *s.get(pos + 6)? as usize;
        let size_at = pos + 6 + (name_len + 2) / 2 * 2;
        let size = u32::from_be_bytes(s.get(size_at..size_at + 4)?.try_into().ok()?) as usize;
        let data = size_at + 4..(size_at + 4).checked_add(size)?;
        s.get(data.clone())?;
        // Odd sizes are padded to even, though some writers leave that out.
        let pad = size % 2 == 1 && s.get(data.end) == Some(&0);
        let end = data.end + usize::from(pad);
        out.push(Irb { id, span: pos..end, data });
        pos = end;
    }
    Some(out)
}

fn photoshop_stream<'a>(payloads: impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
    payloads.filter_map(|p| p.strip_prefix(PHOTOSHOP_HEADER)).flatten().copied().collect()
}

// ---------------------------------------------------------------------------
// IPTC

#[derive(Clone, PartialEq, Debug)]
struct Dataset {
    id: (u8, u8),
    data: Vec<u8>,
}

#[derive(PartialEq)]
enum Charset {
    Utf8,
    /// No 1:90 dataset. Most such text is CP1252; some writers use UTF-8 without saying so.
    Undeclared { utf8: bool },
    Other,
}

fn parse_datasets(d: &[u8]) -> Option<(Vec<Dataset>, Vec<u8>)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while d.get(pos) == Some(&0x1C) {
        let h = d.get(pos + 1..pos + 5)?;
        let mut len = u16::from_be_bytes([h[2], h[3]]) as usize;
        pos += 5;
        if len & 0x8000 != 0 {
            let n = len & 0x7FFF;
            if n > 4 {
                return None;
            }
            len = d.get(pos..pos + n)?.iter().fold(0, |v, &b| v << 8 | b as usize);
            pos += n;
        }
        out.push(Dataset { id: (h[0], h[1]), data: d.get(pos..pos.checked_add(len)?)?.to_vec() });
        pos += len;
    }
    Some((out, d[pos..].to_vec()))
}

fn encode_datasets(sets: &[Dataset], tail: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for s in sets {
        out.extend_from_slice(&[0x1C, s.id.0, s.id.1]);
        if s.data.len() > 0x7FFF {
            out.extend_from_slice(&0x8004u16.to_be_bytes());
            out.extend_from_slice(&(s.data.len() as u32).to_be_bytes());
        } else {
            out.extend_from_slice(&(s.data.len() as u16).to_be_bytes());
        }
        out.extend_from_slice(&s.data);
    }
    out.extend_from_slice(tail);
    out
}

fn charset(sets: &[Dataset]) -> Charset {
    match sets.iter().find(|s| s.id == IPTC_CHARSET) {
        Some(s) if s.data == ESC_UTF8 => Charset::Utf8,
        Some(_) => Charset::Other,
        None => Charset::Undeclared { utf8: sets.iter().filter(|s| is_text(s)).all(|s| std::str::from_utf8(&s.data).is_ok()) },
    }
}

fn is_text(s: &Dataset) -> bool {
    s.id.0 == 2 && IPTC_TEXT.contains(&s.id.1)
}

fn cp1252(bytes: &[u8]) -> String {
    const HIGH: [u32; 32] = [
        0x20AC, 0x81, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x8D, 0x017D,
        0x8F, 0x90, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x9D,
        0x017E, 0x0178,
    ];
    let to_char = |b: u8| match b {
        0x80..=0x9F => char::from_u32(HIGH[b as usize - 0x80]).unwrap_or('\u{FFFD}'),
        _ => b as char,
    };
    bytes.iter().copied().map(to_char).collect()
}

fn decode(data: &[u8], cs: &Charset) -> String {
    match cs {
        Charset::Undeclared { utf8: false } => cp1252(data),
        _ => String::from_utf8_lossy(data).into_owned(),
    }
}

fn keywords_of(sets: &[Dataset]) -> Vec<String> {
    let cs = charset(sets);
    sets.iter().filter(|s| s.id == IPTC_KEYWORDS).map(|s| decode(&s.data, &cs)).collect()
}

/// IPTC keywords from the payloads of a JPEG's APP13 segments.
pub fn read_iptc<'a>(app13s: impl Iterator<Item = &'a [u8]>) -> Vec<String> {
    let stream = photoshop_stream(app13s);
    let block = parse_irbs(&stream).and_then(|irbs| irbs.into_iter().find(|b| b.id == IRB_IPTC));
    block
        .and_then(|b| parse_datasets(&stream[b.data]))
        .map_or(Vec::new(), |(sets, _)| keywords_of(&sets))
}

/// Keeps datasets in ID order, after any others with the same ID.
fn insert_dataset(sets: &mut Vec<Dataset>, id: (u8, u8), data: Vec<u8>) {
    let at = sets.iter().rposition(|s| s.id <= id).map_or(0, |i| i + 1);
    sets.insert(at, Dataset { id, data });
}

/// Returns the new IPTC block, or `None` if nothing has to change. `seed` is what
/// to start from when there are no IPTC keywords yet.
fn edit_iptc(old: &[u8], keyword: &str, on: bool, seed: &[String]) -> io::Result<Option<Vec<u8>>> {
    let (mut sets, tail) = parse_datasets(old).ok_or_else(|| invalid("corrupt IPTC data"))?;
    let existing = keywords_of(&sets);
    if !on {
        if !contains(&existing, keyword) {
            return Ok(None);
        }
        let cs = charset(&sets);
        sets.retain(|s| s.id != IPTC_KEYWORDS || !same_keyword(&decode(&s.data, &cs), keyword));
        return Ok(Some(encode_datasets(&sets, &tail)));
    }
    if contains(&existing, keyword) {
        return Ok(None);
    }
    match charset(&sets) {
        Charset::Utf8 => {}
        Charset::Other => return Err(invalid("IPTC uses a character set other than UTF-8; file left unchanged")),
        cs @ Charset::Undeclared { .. } => {
            for s in sets.iter_mut().filter(|s| is_text(s)) {
                s.data = decode(&s.data, &cs).into_bytes();
            }
            insert_dataset(&mut sets, IPTC_CHARSET, ESC_UTF8.to_vec());
        }
    }
    if !sets.iter().any(|s| s.id == IPTC_RECORD_VERSION) {
        insert_dataset(&mut sets, IPTC_RECORD_VERSION, RECORD_VERSION_4.to_vec());
    }
    if existing.is_empty() {
        // Longer keywords aren't legal in IPTC; they stay in XMP.
        for k in seed.iter().filter(|k| k.len() <= MAX_KEYWORD_BYTES && !same_keyword(k, keyword)) {
            insert_dataset(&mut sets, IPTC_KEYWORDS, k.as_bytes().to_vec());
        }
    }
    insert_dataset(&mut sets, IPTC_KEYWORDS, keyword.as_bytes().to_vec());
    Ok(Some(encode_datasets(&sets, &tail)))
}

/// Replaces (or adds) the IPTC block, keeping the other resources byte for byte.
fn stream_with_iptc(stream: &[u8], irbs: &[Irb], iptc: &[u8]) -> Vec<u8> {
    let mut block = b"8BIM".to_vec();
    block.extend_from_slice(&IRB_IPTC.to_be_bytes());
    block.extend_from_slice(&[0, 0]);
    block.extend_from_slice(&(iptc.len() as u32).to_be_bytes());
    block.extend_from_slice(iptc);
    if iptc.len() % 2 == 1 {
        block.push(0);
    }
    let digest = md5::compute(iptc).0;
    let mut out = Vec::with_capacity(stream.len() + block.len());
    let mut pos = 0;
    let mut placed = false;
    for b in irbs {
        out.extend_from_slice(&stream[pos..b.span.start]);
        if b.id == IRB_IPTC {
            out.extend_from_slice(&block);
            placed = true;
        } else if b.id == IRB_IPTC_DIGEST && b.data.len() == digest.len() {
            // Tools compare this with the block to detect stale IPTC.
            out.extend_from_slice(&stream[b.span.start..b.data.start]);
            out.extend_from_slice(&digest);
            out.extend_from_slice(&stream[b.data.end..b.span.end]);
        } else {
            out.extend_from_slice(&stream[b.span.clone()]);
        }
        pos = b.span.end;
    }
    if !placed {
        out.extend_from_slice(&block);
    }
    out.extend_from_slice(&stream[pos..]);
    out
}

// ---------------------------------------------------------------------------
// JPEG

/// The keyword-related content of a JPEG.
struct Stores {
    /// `None` without a `dc:subject`.
    xmp: Option<Vec<String>>,
    iptc: Vec<Dataset>,
    /// Every resource but IPTC and its digest.
    other_irbs: Vec<Vec<u8>>,
}

fn is_xmp(buf: &[u8], s: &Segment) -> bool {
    s.marker == 0xE1 && buf[s.data..s.end].starts_with(XMP_HEADER)
}

fn is_photoshop(buf: &[u8], s: &Segment) -> bool {
    s.marker == MARKER_APP13 && buf[s.data..s.end].starts_with(PHOTOSHOP_HEADER)
}

fn is_mpf(buf: &[u8], s: &Segment) -> bool {
    s.marker == 0xE2 && buf[s.data..s.end].starts_with(b"MPF\0")
}

fn xmp_text<'a>(buf: &'a [u8], segs: &[Segment]) -> io::Result<Option<&'a str>> {
    let mut xmp = segs.iter().filter(|s| is_xmp(buf, s));
    let first = xmp.next();
    if xmp.next().is_some() {
        return Err(invalid("more than one XMP segment"));
    }
    first
        .map(|s| std::str::from_utf8(&buf[s.data + XMP_HEADER.len()..s.end]).map_err(|_| invalid("XMP is not UTF-8")))
        .transpose()
}

fn photoshop(buf: &[u8], segs: &[Segment]) -> io::Result<(Vec<u8>, Vec<Irb>)> {
    let stream = photoshop_stream(segs.iter().filter(|s| is_photoshop(buf, s)).map(|s| &buf[s.data..s.end]));
    let irbs = parse_irbs(&stream).ok_or_else(|| invalid("corrupt Photoshop APP13 segment"))?;
    if irbs.iter().filter(|b| b.id == IRB_IPTC).count() > 1 {
        return Err(invalid("more than one IPTC block"));
    }
    Ok((stream, irbs))
}

fn stores(buf: &[u8]) -> io::Result<Stores> {
    let (segs, _) = scan_segments(buf)?;
    let (stream, irbs) = photoshop(buf, &segs)?;
    let iptc = match irbs.iter().find(|b| b.id == IRB_IPTC) {
        Some(b) => parse_datasets(&stream[b.data.clone()]).ok_or_else(|| invalid("corrupt IPTC data"))?.0,
        None => Vec::new(),
    };
    let xmp = xmp_text(buf, &segs)?.map(scan_xmp).transpose()?;
    Ok(Stores {
        xmp: xmp.and_then(|x| x.subject).map(|s| s.items.into_iter().map(|i| i.text).collect()),
        iptc,
        other_irbs: irbs
            .iter()
            .filter(|b| b.id != IRB_IPTC && b.id != IRB_IPTC_DIGEST)
            .map(|b| stream[b.span.clone()].to_vec())
            .collect(),
    })
}

/// Returns a copy of the JPEG with `keyword` added to or removed from XMP and
/// IPTC, or `None` if the file is already that way.
pub fn jpeg_with_keyword(buf: &[u8], keyword: &str, on: bool) -> io::Result<Option<Vec<u8>>> {
    let (segs, sos) = scan_segments(buf)?;
    let (stream, irbs) = photoshop(buf, &segs)?;
    let old_xmp = xmp_text(buf, &segs)?;
    let old = stores(buf)?;
    let iptc_keywords = keywords_of(&old.iptc);

    let xmp_seed = if old.xmp.is_none() { iptc_keywords.as_slice() } else { &[] };
    let new_xmp = edit_xmp(old_xmp.unwrap_or(EMPTY_XMP), keyword, on, xmp_seed)?;
    let iptc_block = irbs.iter().find(|b| b.id == IRB_IPTC).map_or(&[][..], |b| &stream[b.data.clone()]);
    let iptc_seed = old.xmp.clone().unwrap_or_default();
    let new_iptc = edit_iptc(iptc_block, keyword, on, &iptc_seed)?;
    if new_xmp.is_none() && new_iptc.is_none() {
        return Ok(None);
    }
    let new_stream = new_iptc.map(|iptc| stream_with_iptc(&stream, &irbs, &iptc));

    let push_xmp = |out: &mut Vec<u8>, xmp: &str| push_segment(out, 0xE1, XMP_HEADER, xmp.as_bytes());
    let push_app13 = |out: &mut Vec<u8>, stream: &[u8]| {
        stream.chunks(MAX_APP13_PAYLOAD).try_for_each(|c| push_segment(out, MARKER_APP13, PHOTOSHOP_HEADER, c))
    };

    // New segments go after EXIF, ahead of APP2: an MPF segment there holds offsets
    // to the images after this one, measured from itself.
    let new_at = segs.iter().take_while(|s| matches!(s.marker, 0xE0 | 0xE1)).count();
    let has_xmp = segs.iter().any(|s| is_xmp(buf, s));
    let has_app13 = segs.iter().any(|s| is_photoshop(buf, s));
    let mut out = Vec::with_capacity(buf.len() + 4096);
    out.extend_from_slice(&[0xFF, 0xD8]);
    let mut app13_written = false;
    for i in 0..=segs.len() {
        if i == new_at {
            if let (false, Some(x)) = (has_xmp, &new_xmp) {
                push_xmp(&mut out, x)?;
            }
            if let (false, Some(s)) = (has_app13, &new_stream) {
                push_app13(&mut out, s)?;
            }
        }
        let Some(s) = segs.get(i) else { break };
        match (&new_xmp, &new_stream) {
            (Some(x), _) if is_xmp(buf, s) => push_xmp(&mut out, x)?,
            (_, Some(ps)) if is_photoshop(buf, s) => {
                if !app13_written {
                    push_app13(&mut out, ps)?;
                    app13_written = true;
                }
            }
            _ => out.extend_from_slice(&buf[s.start..s.end]),
        }
    }
    out.extend_from_slice(&buf[sos..]);
    verify(buf, &out, &old, keyword, on)?;
    Ok(Some(out))
}

/// Everything that has to survive: the image data, all other segments, the
/// other keywords and IPTC datasets, the other Photoshop resources.
fn verify(old: &[u8], new: &[u8], before: &Stores, keyword: &str, on: bool) -> io::Result<()> {
    let fail = |what: &str| invalid(&format!("verification failed ({what}); file left unchanged"));
    let (old_segs, old_sos) = scan_segments(old)?;
    let (new_segs, new_sos) = scan_segments(new)?;
    if old[old_sos..] != new[new_sos..] {
        return Err(fail("image data"));
    }
    let others = |buf: &'_ [u8], segs: &[Segment]| -> Vec<Vec<u8>> {
        let kept = segs.iter().filter(|s| !is_xmp(buf, s) && !is_photoshop(buf, s));
        kept.map(|s| buf[s.start..s.end].to_vec()).collect()
    };
    if others(old, &old_segs) != others(new, &new_segs) {
        return Err(fail("other metadata"));
    }
    // MPF offsets are relative to the segment, so its distance to the image must not change.
    let mpf_tail = |buf: &[u8], segs: &[Segment], sos: usize| segs.iter().find(|s| is_mpf(buf, s)).map(|s| sos - s.start);
    if mpf_tail(old, &old_segs, old_sos) != mpf_tail(new, &new_segs, new_sos) {
        return Err(invalid("XMP or IPTC sits after the multi-picture (MPF) segment; file left unchanged"));
    }

    let after = stores(new).map_err(|_| fail("reading the result"))?;
    if after.other_irbs != before.other_irbs {
        return Err(fail("Photoshop resources"));
    }
    let others_of = |list: &[String]| -> Vec<String> { list.iter().filter(|k| !same_keyword(k, keyword)).cloned().collect() };
    let (old_iptc, new_iptc) = (keywords_of(&before.iptc), keywords_of(&after.iptc));
    let new_xmp = after.xmp.clone().unwrap_or_default();
    if contains(&new_xmp, keyword) != on || contains(&new_iptc, keyword) != on {
        return Err(fail("keyword not written"));
    }
    if let Some(old_xmp) = &before.xmp {
        if others_of(old_xmp) != others_of(&new_xmp) {
            return Err(fail("other XMP keywords"));
        }
    }
    if !old_iptc.is_empty() && others_of(&old_iptc) != others_of(&new_iptc) {
        return Err(fail("other IPTC keywords"));
    }
    let shape = |sets: &[Dataset]| -> Vec<(u8, u8)> {
        let keep = |id: &(u8, u8)| ![IPTC_KEYWORDS, IPTC_CHARSET, IPTC_RECORD_VERSION].contains(id);
        sets.iter().map(|s| s.id).filter(keep).collect()
    };
    if shape(&before.iptc) != shape(&after.iptc) {
        return Err(fail("other IPTC data"));
    }
    if meta::read_meta_from_bytes(old).rating != meta::read_meta_from_bytes(new).rating {
        return Err(fail("rating"));
    }
    Ok(())
}

/// Adds or removes the keyword in the file on disk, atomically (temp file +
/// rename), keeping everything else about the file. Merges with what is in the
/// file now, so tags set elsewhere since the app read it are kept.
pub fn write_keyword(path: &Path, keyword: &str, on: bool) -> io::Result<()> {
    if crate::heic::is_heic(path) {
        return Err(invalid("keywords are only supported on JPEG files"));
    }
    let buf = fs::read(path)?;
    match jpeg_with_keyword(&buf, keyword, on)? {
        Some(new) => meta::replace_file(path, &new, None),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{EXIF_HEADER, read_meta_from_bytes, read_meta_from_file, write_rating};
    use std::path::PathBuf;
    use std::process::Command;

    const SCAN: [u8; 9] = [0xFF, 0xDA, 0x00, 0x02, 1, 2, 3, 0xFF, 0xD9];
    const KW: &str = "pere";

    fn jpeg(segments: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        for (m, body) in segments {
            push_segment(&mut v, *m, &[], body).unwrap();
        }
        v.extend_from_slice(&SCAN);
        v
    }

    fn xmp_segment(xmp: &str) -> (u8, Vec<u8>) {
        (0xE1, [XMP_HEADER, xmp.as_bytes()].concat())
    }

    fn exif_segment() -> (u8, Vec<u8>) {
        // Little-endian TIFF with IFD0 = { Orientation = 6 }.
        let mut tiff = b"II*\0\x08\0\0\0\x01\0".to_vec();
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&[6, 0, 0, 0, 0, 0, 0, 0]);
        (0xE1, [EXIF_HEADER, &tiff].concat())
    }

    fn irb(id: u16, data: &[u8]) -> Vec<u8> {
        let mut v = b"8BIM".to_vec();
        v.extend_from_slice(&id.to_be_bytes());
        v.extend_from_slice(&[0, 0]);
        v.extend_from_slice(&(data.len() as u32).to_be_bytes());
        v.extend_from_slice(data);
        if data.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    fn app13(stream: &[u8]) -> (u8, Vec<u8>) {
        (MARKER_APP13, [PHOTOSHOP_HEADER, stream].concat())
    }

    fn iptc(sets: &[((u8, u8), &[u8])]) -> Vec<u8> {
        let sets: Vec<Dataset> = sets.iter().map(|(id, d)| Dataset { id: *id, data: d.to_vec() }).collect();
        encode_datasets(&sets, &[])
    }

    fn bag(items: &[&str]) -> String {
        let lis: String = items.iter().map(|k| format!("\n     <rdf:li>{k}</rdf:li>")).collect();
        format!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"{RDF_NS}\">\n  \
             <rdf:Description rdf:about=\"\" xmlns:dc=\"{DC_NS}\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmp:Rating=\"3\">\n   \
             <dc:title><rdf:Alt><rdf:li xml:lang=\"x-default\">Beach</rdf:li></rdf:Alt></dc:title>\n   \
             <dc:subject>\n    <rdf:Bag>{lis}\n    </rdf:Bag>\n   </dc:subject>\n  \
             </rdf:Description>\n </rdf:RDF></x:xmpmeta>"
        )
    }

    fn xmp_list(buf: &[u8]) -> Option<Vec<String>> {
        stores(buf).unwrap().xmp
    }

    fn iptc_list(buf: &[u8]) -> Vec<String> {
        keywords_of(&stores(buf).unwrap().iptc)
    }

    fn set(buf: &[u8], k: &str, on: bool) -> Vec<u8> {
        jpeg_with_keyword(buf, k, on).unwrap().expect("a change")
    }

    /// Everything from the scan on, and every segment but XMP and Photoshop.
    fn untouched(buf: &[u8]) -> (Vec<Vec<u8>>, Vec<u8>) {
        let (segs, sos) = scan_segments(buf).unwrap();
        let kept = segs.iter().filter(|s| !is_xmp(buf, s) && !is_photoshop(buf, s));
        (kept.map(|s| buf[s.start..s.end].to_vec()).collect(), buf[sos..].to_vec())
    }

    #[test]
    fn add_remove_readd_keeps_other_keywords_and_order() {
        let keep = iptc(&[((1, 90), ESC_UTF8), ((2, 0), &RECORD_VERSION_4), ((2, 25), b"zebra"), ((2, 25), b"apple"), ((2, 120), b"Caption")]);
        let photo = jpeg(&[exif_segment(), xmp_segment(&bag(&["zebra", "apple"])), app13(&irb(IRB_IPTC, &keep))]);

        let on = set(&photo, KW, true);
        assert_eq!(xmp_list(&on).unwrap(), ["zebra", "apple", KW]);
        assert_eq!(iptc_list(&on), ["zebra", "apple", KW]);
        assert_eq!(untouched(&on), untouched(&photo));
        assert!(jpeg_with_keyword(&on, KW, true).unwrap().is_none(), "no duplicates");

        let off = set(&on, KW, false);
        assert_eq!(xmp_list(&off).unwrap(), ["zebra", "apple"]);
        assert_eq!(iptc_list(&off), ["zebra", "apple"]);
        assert_eq!(off, photo, "removing restores the original bytes");
        assert!(jpeg_with_keyword(&off, KW, false).unwrap().is_none());

        let again = set(&off, KW, true);
        assert_eq!(again, on);
        assert_eq!(read_meta_from_bytes(&again).keywords, ["zebra", "apple", KW]);
        assert_eq!(read_meta_from_bytes(&again).rating, Some(3));
        assert_eq!(read_meta_from_bytes(&again).orientation, 6);
    }

    #[test]
    fn creates_everything_on_a_bare_jpeg() {
        let photo = jpeg(&[(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0".to_vec()), exif_segment()]);
        let on = set(&photo, KW, true);
        assert_eq!(xmp_list(&on).unwrap(), [KW]);
        assert_eq!(iptc_list(&on), [KW]);
        let sets = stores(&on).unwrap().iptc;
        assert_eq!(sets[0], Dataset { id: IPTC_CHARSET, data: ESC_UTF8.to_vec() });
        assert_eq!(sets[1], Dataset { id: IPTC_RECORD_VERSION, data: RECORD_VERSION_4.to_vec() });
        let (segs, _) = scan_segments(&on).unwrap();
        assert_eq!(segs.iter().map(|s| s.marker).collect::<Vec<_>>(), [0xE0, 0xE1, 0xE1, MARKER_APP13]);
        assert_eq!(untouched(&on), untouched(&photo));

        let off = set(&on, KW, false);
        assert_eq!(xmp_list(&off), None, "an empty dc:subject is dropped");
        assert!(iptc_list(&off).is_empty());
        assert_eq!(read_meta_from_bytes(&off).keywords, Vec::<String>::new());
    }

    #[test]
    fn iptc_only_keywords_stay_visible_when_xmp_is_created() {
        let block = iptc(&[((2, 25), b"old")]);
        let photo = jpeg(&[app13(&irb(IRB_IPTC, &block))]);
        let on = set(&photo, KW, true);
        // XMP wins in Synology, so without this "old" would disappear from it.
        assert_eq!(xmp_list(&on).unwrap(), ["old", KW]);
        assert_eq!(iptc_list(&on), ["old", KW]);
    }

    #[test]
    fn xmp_only_keywords_are_copied_to_new_iptc() {
        let photo = jpeg(&[xmp_segment(&bag(&["old", &"x".repeat(MAX_KEYWORD_BYTES + 1)]))]);
        let on = set(&photo, KW, true);
        assert_eq!(iptc_list(&on), ["old", KW], "too long for IPTC: stays in XMP only");
        assert_eq!(xmp_list(&on).unwrap().len(), 3);
    }

    #[test]
    fn stores_that_disagree_are_not_synced() {
        let block = iptc(&[((2, 25), b"iptc-only")]);
        let photo = jpeg(&[xmp_segment(&bag(&["xmp-only"])), app13(&irb(IRB_IPTC, &block))]);
        let on = set(&photo, KW, true);
        assert_eq!(xmp_list(&on).unwrap(), ["xmp-only", KW]);
        assert_eq!(iptc_list(&on), ["iptc-only", KW]);
        // Only one store has it: the missing one still gets it.
        let half = jpeg(&[xmp_segment(&bag(&[KW])), app13(&irb(IRB_IPTC, &block))]);
        assert_eq!(iptc_list(&set(&half, KW, true)), ["iptc-only", KW]);
    }

    #[test]
    fn case_and_unicode_form_do_not_make_duplicates() {
        let photo = jpeg(&[xmp_segment(&bag(&["Pere", "o\u{303}de"]))]);
        let on = set(&photo, KW, true);
        assert_eq!(xmp_list(&on).unwrap(), ["Pere", "o\u{303}de"]);
        assert!(contains(&xmp_list(&on).unwrap(), "õde"));
        assert_eq!(xmp_list(&set(&on, "PERE", false)).unwrap(), ["o\u{303}de"]);
    }

    #[test]
    fn estonian_and_special_characters_round_trip() {
        let k = "Õnne & sõbrad <ä ö ü>";
        let on = set(&jpeg(&[]), k, true);
        assert_eq!(xmp_list(&on).unwrap(), [k]);
        assert_eq!(iptc_list(&on), [k]);
        let xmp = xmp_text(&on, &scan_segments(&on).unwrap().0).unwrap().unwrap();
        assert!(xmp.contains("Õnne &amp; sõbrad &lt;ä ö ü&gt;"), "{xmp}");
        assert_eq!(read_meta_from_bytes(&on).keywords, [k]);
        assert!(set(&on, k, false).len() < on.len());
    }

    #[test]
    fn undeclared_latin_text_is_converted_when_marking_utf8() {
        let block = iptc(&[((2, 25), b"T\xF5nu"), ((2, 120), b"\x80 m\xE4gi"), ((2, 125), b"\xFF\xFE")]);
        let photo = jpeg(&[app13(&irb(IRB_IPTC, &block))]);
        assert_eq!(read_meta_from_bytes(&photo).keywords, ["Tõnu"]);
        let on = set(&photo, KW, true);
        let sets = stores(&on).unwrap().iptc;
        assert!(charset(&sets) == Charset::Utf8);
        let data = |id| sets.iter().find(|s| s.id == id).unwrap().data.clone();
        assert_eq!(data((2, 120)), "€ mägi".as_bytes());
        assert_eq!(data((2, 125)), b"\xFF\xFE", "binary data is left alone");
        assert_eq!(iptc_list(&on), ["Tõnu", KW]);
    }

    #[test]
    fn undeclared_utf8_text_is_not_converted_twice() {
        let block = iptc(&[((2, 25), "Tõnu".as_bytes())]);
        let on = set(&jpeg(&[app13(&irb(IRB_IPTC, &block))]), KW, true);
        assert_eq!(iptc_list(&on), ["Tõnu", KW]);
    }

    #[test]
    fn other_charset_is_refused() {
        let block = iptc(&[((1, 90), b"\x1B.A"), ((2, 25), b"T\xF5nu")]);
        let photo = jpeg(&[app13(&irb(IRB_IPTC, &block))]);
        let err = jpeg_with_keyword(&photo, KW, true).unwrap_err();
        assert!(err.to_string().contains("character set"), "{err}");
    }

    #[test]
    fn other_photoshop_resources_survive_and_digest_is_updated() {
        let block = iptc(&[((2, 25), b"a")]);
        let resolution = irb(0x03ED, &[1, 2, 3, 4, 5, 6, 7]);
        let digest = irb(IRB_IPTC_DIGEST, &md5::compute(&block).0);
        // A named resource: length byte + name, padded to even.
        let named = b"8BIM\x04\x0A\x03abc\0\0\0\x02hi".to_vec();
        let stream = [resolution.clone(), named.clone(), irb(IRB_IPTC, &block), digest].concat();
        let photo = jpeg(&[app13(&stream)]);
        let on = set(&photo, KW, true);

        let (segs, _) = scan_segments(&on).unwrap();
        let (out, irbs) = photoshop(&on, &segs).unwrap();
        assert_eq!(out[irbs[0].span.clone()], resolution[..]);
        assert_eq!(out[irbs[1].span.clone()], named[..]);
        let new_block = &out[irbs[2].data.clone()];
        assert_eq!(out[irbs[3].data.clone()], md5::compute(new_block).0);
    }

    #[test]
    fn resources_without_padding_after_odd_data_are_read() {
        // Seen in real files: the next block follows odd-sized IPTC directly.
        let block = iptc(&[((2, 25), b"abc"), ((2, 120), b"xy")]);
        assert_eq!(block.len() % 2, 1);
        let mut unpadded = irb(IRB_IPTC, &block);
        unpadded.pop();
        let stream = [unpadded, irb(IRB_IPTC_DIGEST, &md5::compute(&block).0)].concat();
        let photo = jpeg(&[app13(&stream)]);
        assert_eq!(read_meta_from_bytes(&photo).keywords, ["abc"]);
        let on = set(&photo, KW, true);
        assert_eq!(iptc_list(&on), ["abc", KW]);
        let (segs, _) = scan_segments(&on).unwrap();
        let (out, irbs) = photoshop(&on, &segs).unwrap();
        assert_eq!(out[irbs[1].data.clone()], md5::compute(&out[irbs[0].data.clone()]).0);
    }

    #[test]
    fn large_resources_span_several_segments() {
        let big = irb(0x040C, &vec![7; 100_000]);
        let block = iptc(&[((2, 25), b"a")]);
        let stream = [big.clone(), irb(IRB_IPTC, &block)].concat();
        let mut parts = stream.chunks(MAX_APP13_PAYLOAD).map(app13).collect::<Vec<_>>();
        parts.insert(1, (0xFE, b"comment".to_vec()));
        let photo = jpeg(&parts);
        let on = set(&photo, KW, true);
        assert_eq!(iptc_list(&on), ["a", KW]);
        assert_eq!(stores(&on).unwrap().other_irbs, [big]);
        assert_eq!(untouched(&on), untouched(&photo));
    }

    #[test]
    fn xmp_forms_from_other_writers() {
        // Another prefix for dc, a CDATA entry, an entity, an empty bag, a Seq.
        let cases = [
            (
                format!("<rdf:RDF xmlns:rdf=\"{RDF_NS}\"><rdf:Description xmlns:ns0=\"{DC_NS}\"><ns0:subject><rdf:Bag><rdf:li><![CDATA[a&b]]></rdf:li><rdf:li>c &amp; d</rdf:li></rdf:Bag></ns0:subject></rdf:Description></rdf:RDF>"),
                vec!["a&b", "c & d", KW],
            ),
            (format!("<r:RDF xmlns:r=\"{RDF_NS}\"><r:Description xmlns:dc=\"{DC_NS}\"><dc:subject><r:Bag/></dc:subject></r:Description></r:RDF>"), vec![KW]),
            (format!("<rdf:RDF xmlns:rdf=\"{RDF_NS}\"><rdf:Description xmlns:dc=\"{DC_NS}\"><dc:subject><rdf:Bag></rdf:Bag></dc:subject></rdf:Description></rdf:RDF>"), vec![KW]),
            (format!("<rdf:RDF xmlns:rdf=\"{RDF_NS}\"><rdf:Description xmlns:dc=\"{DC_NS}\"><dc:subject><rdf:Seq><rdf:li>s</rdf:li></rdf:Seq></dc:subject></rdf:Description></rdf:RDF>"), vec!["s", KW]),
            // No dc:subject: a new description repeats the document's rdf:about.
            (format!("<r:RDF xmlns:r=\"{RDF_NS}\"><r:Description r:about=\"uuid:1\"/></r:RDF>"), vec![KW]),
        ];
        for (xmp, want) in cases {
            let on = set(&jpeg(&[xmp_segment(&xmp)]), KW, true);
            assert_eq!(xmp_list(&on).unwrap(), want, "{xmp}");
            if !xmp.contains("subject") {
                let new = xmp_text(&on, &scan_segments(&on).unwrap().0).unwrap().unwrap();
                assert!(new.contains("<r:Description r:about=\"uuid:1\" xmlns:dc"), "{new}");
            }
        }
    }

    #[test]
    fn unusual_or_broken_xmp_is_refused() {
        let rdf = |body: &str| format!("<rdf:RDF xmlns:rdf=\"{RDF_NS}\" xmlns:dc=\"{DC_NS}\">{body}</rdf:RDF>");
        for bad in [
            rdf("<rdf:Description><dc:subject>plain</dc:subject></rdf:Description>"),
            rdf("<rdf:Description><dc:subject/></rdf:Description>"),
            rdf("<rdf:Description><dc:subject></dc:subject></rdf:Description>"),
            rdf("<rdf:Description dc:subject=\"x\"/>"),
            rdf("<rdf:Description><dc:subject><rdf:Alt><rdf:li>a</rdf:li></rdf:Alt></dc:subject></rdf:Description>"),
            rdf("<rdf:Description><dc:subject><rdf:Bag/></dc:subject><dc:subject><rdf:Bag/></dc:subject></rdf:Description>"),
            rdf("<rdf:Description><dc:subject><rdf:Bag><rdf:li>x</rdf:Bag></dc:subject></rdf:Description>"),
            rdf("<rdf:Description><dc:subject><rdf:Bag>"),
            "not xml at all <<".to_string(),
        ] {
            assert!(jpeg_with_keyword(&jpeg(&[xmp_segment(&bad)]), KW, true).is_err(), "{bad}");
        }
        let utf16 = [XMP_HEADER, &[0xFF, 0xFE, b'<', 0]].concat();
        assert!(jpeg_with_keyword(&jpeg(&[(0xE1, utf16)]), KW, true).is_err());
        let two = jpeg(&[xmp_segment(&bag(&[])), xmp_segment(&bag(&[]))]);
        assert!(jpeg_with_keyword(&two, KW, true).is_err());
        assert!(jpeg_with_keyword(&jpeg(&[app13(b"8BIM\x04")]), KW, true).is_err());
        assert!(jpeg_with_keyword(b"not a jpeg", KW, true).is_err());
    }

    #[test]
    fn multi_picture_offsets_stay_valid() {
        let mpf = (0xE2, b"MPF\0II*\0".to_vec());
        // New segments go ahead of MPF, so its distance to the image is unchanged.
        let photo = jpeg(&[exif_segment(), mpf.clone()]);
        let on = set(&photo, KW, true);
        let (segs, sos) = scan_segments(&on).unwrap();
        let last = segs.last().unwrap();
        assert!(is_mpf(&on, last) && sos == last.end);
        // Metadata after MPF can't grow without moving what MPF points to.
        let after = jpeg(&[mpf, xmp_segment(&bag(&[]))]);
        let err = jpeg_with_keyword(&after, KW, true).unwrap_err();
        assert!(err.to_string().contains("MPF"), "{err}");
    }

    #[test]
    fn oversized_xmp_is_an_error() {
        let filler = "x".repeat(0xFFFF - XMP_HEADER.len() - bag(&["a"]).len() - 2);
        let photo = jpeg(&[xmp_segment(&bag(&["a"]).replace("Beach", &format!("Beach{filler}")))]);
        assert!(jpeg_with_keyword(&photo, KW, true).is_err());
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("picker-kw-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn list_dir(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    #[test]
    fn file_write_keeps_timestamps_and_leaves_no_temp_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("file");
        let path = dir.join("a.jpg");
        fs::write(&path, jpeg(&[exif_segment()])).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let mtime = filetime::FileTime::from_unix_time(1_000_000_000, 0);
        filetime::set_file_mtime(&path, mtime).unwrap();
        let created = fs::metadata(&path).unwrap().created().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));

        write_keyword(&path, KW, true).unwrap();
        let md = fs::metadata(&path).unwrap();
        assert_eq!(filetime::FileTime::from_last_modification_time(&md), mtime);
        assert_eq!(md.permissions().mode() & 0o777, 0o640);
        assert_eq!(md.created().unwrap(), created);
        assert_eq!(list_dir(&dir), ["a.jpg"]);
        assert_eq!(read_meta_from_file(&path).unwrap().keywords, [KW]);

        // Toggling on again is a no-op: the file isn't rewritten.
        let inode = |p: &Path| std::os::unix::fs::MetadataExt::ino(&fs::metadata(p).unwrap());
        let before = inode(&path);
        write_keyword(&path, KW, true).unwrap();
        assert_eq!(inode(&path), before);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_write_leaves_the_file_as_it_was() {
        let dir = temp_dir("fail");
        let broken = dir.join("broken.jpg");
        fs::write(&broken, b"\xFF\xD8 this is not a JPEG").unwrap();
        assert!(write_keyword(&broken, KW, true).is_err());
        assert_eq!(fs::read(&broken).unwrap(), b"\xFF\xD8 this is not a JPEG");

        let bad_xmp = dir.join("bad.jpg");
        let bytes = jpeg(&[xmp_segment("<rdf:RDF")]);
        fs::write(&bad_xmp, &bytes).unwrap();
        assert!(write_keyword(&bad_xmp, KW, true).is_err());
        assert_eq!(fs::read(&bad_xmp).unwrap(), bytes);

        let heic = dir.join("a.HEIC");
        fs::write(&heic, b"photo").unwrap();
        assert!(write_keyword(&heic, KW, true).is_err());
        assert_eq!(list_dir(&dir), ["a.HEIC", "bad.jpg", "broken.jpg"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ratings_and_keywords_leave_each_other_alone() {
        let dir = temp_dir("rating");
        let path = dir.join("a.jpg");
        fs::write(&path, jpeg(&[exif_segment()])).unwrap();
        write_keyword(&path, KW, true).unwrap();
        write_rating(&path, 4).unwrap();
        let m = read_meta_from_file(&path).unwrap();
        assert_eq!((m.rating, m.keywords.as_slice()), (Some(4), &[KW.to_string()][..]));
        assert_eq!(iptc_list(&fs::read(&path).unwrap()), [KW]);
        write_keyword(&path, KW, false).unwrap();
        let m = read_meta_from_file(&path).unwrap();
        assert_eq!((m.rating, m.keywords.len()), (Some(4), 0));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn finder_tags_and_attributes_survive_a_keyword_write() {
        let dir = temp_dir("xattr");
        let path = dir.join("a.jpg");
        fs::write(&path, jpeg(&[])).unwrap();
        write_rating(&path, 4).unwrap();
        xattr::set(&path, "com.example.keep", b"1").unwrap();
        let tags = "com.apple.metadata:_kMDItemUserTags";
        let before = xattr::get(&path, tags).unwrap();
        assert!(before.is_some());
        write_keyword(&path, KW, true).unwrap();
        assert_eq!(xattr::get(&path, tags).unwrap(), before);
        assert_eq!(xattr::get(&path, "com.example.keep").unwrap().as_deref(), Some(&b"1"[..]));
        fs::remove_dir_all(&dir).unwrap();
    }

    fn exiftool(args: &[&str]) -> Option<String> {
        let out = Command::new("exiftool").args(args).output().ok()?;
        assert!(out.status.success(), "exiftool {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// All tags as exiftool sees them, without the ones a keyword write may change.
    fn tag_dump(path: &str) -> Vec<String> {
        // A warning about a malformed block goes away when the block is rewritten.
        let changing = ["ExifTool:Warning", "System:", "File:", "Composite:", "IPTC:Keywords", "IPTC:CodedCharacterSet", "IPTC:ApplicationRecordVersion", "XMP-dc:Subject", "XMP-x:", "XMP-rdf:", "Photoshop:IPTCDigest"];
        exiftool(&["-a", "-G1", "-s", "-n", path])
            .unwrap()
            .lines()
            .map(|l| l.replacen('[', "", 1).replacen(']', ":", 1).split_whitespace().collect::<Vec<_>>().join(" "))
            .map(|l| l.replacen(": ", ":", 1))
            .filter(|l| !changing.iter().any(|c| l.starts_with(c)))
            // The absolute position of a second image moves with the metadata before it.
            .filter(|l| !l.contains(":MPImageStart"))
            .collect()
    }

    /// A stored digest, if any, matches the one exiftool computes from the block.
    fn assert_digest_current(path: &str) {
        let digests = exiftool(&["-s3", "-a", "-IPTCDigest", "-CurrentIPTCDigest", path]).unwrap();
        let digests: Vec<&str> = digests.lines().collect();
        assert!(digests.len() < 2 || digests[0] == digests[1], "{path}: {digests:?}");
    }

    /// Skipped (with a note) when exiftool isn't installed.
    #[test]
    fn exiftool_agrees() {
        if Command::new("exiftool").arg("-ver").output().is_err() {
            eprintln!("exiftool not installed: skipping");
            return;
        }
        let dir = temp_dir("exiftool");
        let path = dir.join("a.jpg");
        let p = path.to_str().unwrap();
        fs::write(&path, jpeg(&[(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0".to_vec())])).unwrap();
        exiftool(&[
            "-q", "-overwrite_original", "-codedcharacterset=utf8",
            "-DateTimeOriginal=2024:06:01 12:30:45", "-Make=FUJIFILM", "-Model=X100VI",
            "-GPSLatitude=59.437", "-GPSLatitudeRef=N", "-GPSLongitude=24.7536", "-GPSLongitudeRef=E",
            "-XMP-xmp:Rating=4", "-EXIF:Rating=4", "-Keywords=õde", "-Keywords=Rand", "-XMP-dc:Subject=õde",
            "-XMP-dc:Subject=Rand", "-Caption-Abstract=Pärnu rand", "-XMP-dc:Title=Suvi", "-IPTCDigest=new", p,
        ]);
        let tags = |p: &str| exiftool(&["-charset", "UTF8", "-iptc:charset=UTF8", "-s3", "-sep", "|", "-XMP-dc:Subject", "-IPTC:Keywords", "-IPTC:CodedCharacterSet", p]).unwrap();
        let before = tag_dump(p);
        assert!(before.iter().any(|l| l.contains("GPSLatitude")), "{before:?}");

        write_keyword(&path, "pere", true).unwrap();
        assert_eq!(tags(p), "õde|Rand|pere\nõde|Rand|pere\nUTF8\n");
        assert_eq!(tag_dump(p), before);
        write_keyword(&path, "pere", false).unwrap();
        assert_eq!(tags(p), "õde|Rand\nõde|Rand\nUTF8\n");
        assert_eq!(tag_dump(p), before);
        write_keyword(&path, "õnnelik", true).unwrap();
        assert_eq!(tags(p), "õde|Rand|õnnelik\nõde|Rand|õnnelik\nUTF8\n");

        // exiftool validates the file, and its digest check agrees with ours.
        let warnings = exiftool(&["-validate", "-warning", "-error", "-a", p]).unwrap();
        assert!(!warnings.contains("IPTC") && !warnings.contains("XMP"), "{warnings}");
        assert_digest_current(p);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Run with: PICKER_TEST_DIR=/path/to/copies cargo test --release keywords -- --ignored --nocapture
    /// Rewrites keywords in the given folder (use copies!).
    #[test]
    #[ignore]
    fn real_files_keyword_roundtrip() {
        let Ok(dir) = std::env::var("PICKER_TEST_DIR") else { return };
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if !crate::workers::is_photo(&path) || crate::heic::is_heic(&path) {
                continue;
            }
            let p = path.to_str().unwrap();
            let orig = fs::read(&path).unwrap();
            let before = tag_dump(p);
            let second_image = |p: &str| Command::new("exiftool").args(["-b", "-MPImage2", p]).output().unwrap().stdout;
            let second_before = second_image(p);
            if let Err(e) = write_keyword(&path, KW, true) {
                // A refusal must leave the file alone; it's reported, not a failure.
                assert_eq!(fs::read(&path).unwrap(), orig, "{p}");
                println!("{p}: REFUSED: {e}");
                continue;
            }
            let on = fs::read(&path).unwrap();
            assert_eq!(untouched(&on), untouched(&orig), "{p}");
            assert_eq!(tag_dump(p), before, "{p}");
            assert_eq!(second_image(p), second_before, "{p}: MPF second image");
            assert_digest_current(p);
            assert!(read_meta_from_file(&path).unwrap().keywords.contains(&KW.to_string()), "{p}");
            write_keyword(&path, KW, false).unwrap();
            assert_eq!(tag_dump(p), before, "{p}");
            write_keyword(&path, KW, true).unwrap();
            println!("{p}: OK, +{} bytes", on.len() as i64 - orig.len() as i64);
        }
    }
}
