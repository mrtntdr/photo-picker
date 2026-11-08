//! Background work: a priority-ordered decode scheduler, a folder scanner and
//! a single writer thread that serializes all file modifications.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::{fs, thread};

use eframe::egui::{self, TextureHandle, TextureOptions};

use crate::decode::{decode_jpeg, decode_jpeg_full};
use crate::heic;
use crate::meta::{self, Meta};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    /// Screen-sized preview.
    Preview,
    /// Full resolution (for 100% zoom).
    Full,
    /// Small filmstrip thumbnail.
    Thumb,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    pub generation: u64,
    pub idx: usize,
    pub kind: Kind,
}

#[derive(Clone)]
pub struct Job {
    pub key: Key,
    pub path: PathBuf,
    /// Target box in physical pixels.
    pub target: [u32; 2],
}

pub enum Msg {
    Scanned { generation: u64, root: PathBuf, paths: Vec<PathBuf> },
    Meta { generation: u64, idx: usize, meta: Meta },
    /// `epoch` is [`Scheduler::hidden_epoch`] from before the texture was created.
    Image { key: Key, target: [u32; 2], epoch: u64, result: Result<TextureHandle, String> },
    /// `count` is how many queued rating commands this write covered.
    Written { generation: u64, idx: usize, rating: u8, count: u32, result: Result<(), String> },
    /// `count` is how many queued keyword commands this write covered.
    KeywordWritten { generation: u64, idx: usize, on: bool, count: u32, result: Result<(), String> },
    CopyProgress { done: usize, total: usize },
    CopyDone { copied: usize, skipped: usize, errors: Vec<String>, dest: PathBuf },
}

// ---------------------------------------------------------------------------
// Decode scheduler

#[derive(Default)]
pub struct QueueState {
    /// Jobs in priority order (front = most important).
    pub jobs: Vec<Job>,
    /// Keys being decoded right now. Cleared by the UI when it receives the result,
    /// so a key is always either queued, in flight, or delivered.
    pub in_flight: HashSet<Key>,
}

pub struct Scheduler {
    pub state: Mutex<QueueState>,
    cv: Condvar,
    /// Bumped by the UI on frames where the window is hidden (and on the first
    /// frame after). eframe drops texture uploads on hidden frames, e.g. during
    /// the macOS full-screen animation, so a texture created before the latest
    /// bump may never have reached the GPU and must be decoded again.
    pub hidden_epoch: AtomicU64,
}

impl Scheduler {
    pub fn start(ctx: egui::Context, tx: Sender<Msg>) -> Arc<Self> {
        let s = Arc::new(Scheduler {
            state: Mutex::new(QueueState::default()),
            cv: Condvar::new(),
            hidden_epoch: AtomicU64::new(0),
        });
        let n = thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8);
        for i in 0..n {
            let s = s.clone();
            let ctx = ctx.clone();
            let tx = tx.clone();
            thread::Builder::new()
                .name(format!("decode-{i}"))
                .spawn(move || s.worker(ctx, tx))
                .expect("spawn decode thread");
        }
        s
    }

    /// Replaces the queue with `jobs`, skipping keys already in flight.
    pub fn set_jobs(&self, jobs: Vec<Job>) {
        let mut st = self.state.lock().unwrap();
        st.jobs = jobs.into_iter().filter(|j| !st.in_flight.contains(&j.key)).collect();
        drop(st);
        self.cv.notify_all();
    }

    pub fn finished(&self, key: &Key) {
        self.state.lock().unwrap().in_flight.remove(key);
    }

    fn worker(&self, ctx: egui::Context, tx: Sender<Msg>) {
        loop {
            let job = {
                let mut st = self.state.lock().unwrap();
                while st.jobs.is_empty() {
                    st = self.cv.wait(st).unwrap();
                }
                let job = st.jobs.remove(0);
                st.in_flight.insert(job.key);
                job
            };
            let epoch = self.hidden_epoch.load(Ordering::SeqCst);
            let result = run_job(&ctx, &job);
            if tx.send(Msg::Image { key: job.key, target: job.target, epoch, result }).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    }
}

fn run_job(ctx: &egui::Context, job: &Job) -> Result<TextureHandle, String> {
    let [w, h] = job.target;
    let max_side = ctx.input(|i| i.max_texture_side) as u32;
    let image = if heic::is_heic(&job.path) {
        let m = meta::read_meta_from_file(&job.path).map_err(|e| e.to_string())?;
        match job.key.kind {
            Kind::Thumb => heic::decode(&job.path, &m, true, w, h, max_side)?,
            Kind::Preview => heic::decode(&job.path, &m, false, w, h, max_side)?,
            Kind::Full => heic::decode(&job.path, &m, false, u32::MAX, u32::MAX, max_side)?,
        }
    } else {
        match job.key.kind {
        Kind::Thumb => {
            // Prefer the embedded EXIF thumbnail: only the header is read.
            let m = meta::read_meta_from_file(&job.path).map_err(|e| e.to_string())?;
            let embedded = m
                .thumbnail
                .as_deref()
                .and_then(|t| decode_jpeg(t, m.orientation, w, h, max_side).ok());
            match embedded {
                Some(img) => img,
                None => {
                    let data = fs::read(&job.path).map_err(|e| e.to_string())?;
                    decode_jpeg(&data, m.orientation, w, h, max_side)?
                }
            }
        }
        Kind::Preview => {
            let data = fs::read(&job.path).map_err(|e| e.to_string())?;
            let m = meta::read_meta_from_bytes(&data);
            decode_jpeg(&data, m.orientation, w, h, max_side)?
        }
        Kind::Full => {
            let data = fs::read(&job.path).map_err(|e| e.to_string())?;
            let m = meta::read_meta_from_bytes(&data);
            decode_jpeg_full(&data, m.orientation, max_side)?
        }
        }
    };
    let options = TextureOptions {
        mipmap_mode: (job.key.kind != Kind::Full).then_some(egui::TextureFilter::Linear),
        ..TextureOptions::LINEAR
    };
    let name = format!("{:?}-{}", job.key.kind, job.key.idx);
    Ok(ctx.load_texture(name, image, options))
}

// ---------------------------------------------------------------------------
// Folder scan

/// JPEG or HEIC.
pub fn is_photo(p: &Path) -> bool {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    // Skip macOS "._" AppleDouble files that litter SD cards.
    if name.starts_with('.') {
        return false;
    }
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("jpg" | "jpeg" | "heic" | "heif" | "hif")
    )
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        let hidden = p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.') || n.starts_with('$'));
        if ft.is_dir() && !hidden && depth < 8 {
            collect(&p, out, depth + 1);
        } else if ft.is_file() && is_photo(&p) {
            out.push(p);
        }
    }
}

/// Recursively finds photos, then reads their metadata in parallel.
pub fn scan(generation: u64, root: PathBuf, ctx: egui::Context, tx: Sender<Msg>) {
    thread::spawn(move || {
        let mut paths = Vec::new();
        if root.is_file() {
            paths.push(root.clone());
        } else {
            collect(&root, &mut paths, 0);
        }
        paths.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
        let _ = tx.send(Msg::Scanned { generation, root, paths: paths.clone() });
        ctx.request_repaint();

        use rayon::prelude::*;
        paths.par_iter().enumerate().for_each_with(tx, |tx, (idx, p)| {
            let mut meta = meta::read_meta_from_file(p).unwrap_or_default();
            meta.thumbnail = None;
            meta.mtime = fs::metadata(p).and_then(|m| m.modified()).ok();
            let _ = tx.send(Msg::Meta { generation, idx, meta });
            if idx % 32 == 0 {
                ctx.request_repaint();
            }
        });
        ctx.request_repaint();
    });
}

/// Compares strings so that "IMG_9.JPG" sorts before "IMG_10.JPG".
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, _) => return std::cmp::Ordering::Less,
            (_, None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = a.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    a.next();
                }
                let mut nb = String::new();
                while let Some(c) = b.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(c);
                    b.next();
                }
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
                a.next();
                b.next();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Writer thread

pub enum WriteCmd {
    Rate { generation: u64, idx: usize, path: PathBuf, rating: u8 },
    Keyword { generation: u64, idx: usize, path: PathBuf, keyword: String, on: bool },
    /// Each file with an optional subfolder of `dest` (e.g. "2016-03").
    Copy { files: Vec<(PathBuf, Option<String>)>, dest: PathBuf },
}

pub fn start_writer(ctx: egui::Context, tx: Sender<Msg>, rx: Receiver<WriteCmd>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("writer".into())
        .spawn(move || {
            while let Ok(first) = rx.recv() {
                // Coalesce queued ratings: rapid key presses on one photo write once.
                let mut rates: BTreeMap<PathBuf, (u64, usize, u8, u32)> = BTreeMap::new();
                let mut keywords: BTreeMap<PathBuf, (u64, usize, String, bool, u32)> = BTreeMap::new();
                let mut copies = Vec::new();
                let mut push = |cmd| match cmd {
                    WriteCmd::Rate { generation, idx, path, rating } => {
                        let count = rates.get(&path).map_or(0, |r| r.3) + 1;
                        rates.insert(path, (generation, idx, rating, count));
                    }
                    WriteCmd::Keyword { generation, idx, path, keyword, on } => {
                        let count = keywords.get(&path).map_or(0, |k| k.4) + 1;
                        keywords.insert(path, (generation, idx, keyword, on, count));
                    }
                    WriteCmd::Copy { files, dest } => copies.push((files, dest)),
                };
                push(first);
                while let Ok(cmd) = rx.try_recv() {
                    push(cmd);
                }
                for (path, (generation, idx, rating, count)) in rates {
                    let result = meta::write_rating(&path, rating).map_err(|e| e.to_string());
                    let _ = tx.send(Msg::Written { generation, idx, rating, count, result });
                    ctx.request_repaint();
                }
                for (path, (generation, idx, keyword, on, count)) in keywords {
                    let result = crate::keywords::write_keyword(&path, &keyword, on).map_err(|e| e.to_string());
                    let _ = tx.send(Msg::KeywordWritten { generation, idx, on, count, result });
                    ctx.request_repaint();
                }
                // Copies run after all ratings and keywords queued before them are on disk.
                for (files, dest) in copies {
                    copy_files(&files, &dest, &ctx, &tx);
                }
            }
        })
        .expect("spawn writer thread")
}

/// Same size and modification time (copies keep it; FAT/SMB may round to 2 s).
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    let secs = |m: &fs::Metadata| filetime::FileTime::from_last_modification_time(m).unix_seconds();
    a.len() == b.len() && secs(a).abs_diff(secs(b)) <= 2
}

fn copy_files(files: &[(PathBuf, Option<String>)], dest: &Path, ctx: &egui::Context, tx: &Sender<Msg>) {
    let (mut copied, mut skipped, mut errors) = (0, 0, Vec::new());
    for (i, (src, sub)) in files.iter().enumerate() {
        let _ = tx.send(Msg::CopyProgress { done: i, total: files.len() });
        ctx.request_repaint();
        let Some(name) = src.file_name() else { continue };
        let dir = match sub {
            Some(sub) => dest.join(sub),
            None => dest.to_path_buf(),
        };
        let res = (|| -> std::io::Result<bool> {
            fs::create_dir_all(&dir)?;
            let src_md = fs::metadata(src)?;
            // iPhones reuse names like IMG_1234.HEIC, so a name match alone isn't a duplicate.
            let mut target = dir.join(name);
            let mut n = 1;
            let mut already = false;
            while let Ok(m) = fs::metadata(&target) {
                if same_file(&m, &src_md) {
                    already = true;
                    break;
                }
                let stem = Path::new(name).file_stem().unwrap_or_default().to_string_lossy();
                let ext = Path::new(name).extension().unwrap_or_default().to_string_lossy();
                target = dir.join(format!("{stem}_{n}.{ext}"));
                n += 1;
            }
            if !already {
                let tmp = dir.join(format!(".{}.picker-tmp", name.to_string_lossy()));
                let res = fs::copy(src, &tmp).and_then(|_| {
                    let _ = filetime::set_file_mtime(&tmp, filetime::FileTime::from_last_modification_time(&src_md));
                    fs::rename(&tmp, &target)
                });
                if let Err(e) = res {
                    let _ = fs::remove_file(&tmp);
                    return Err(e);
                }
            }
            // The rating sidecar travels with the photo (and is refreshed if it changed).
            let side = meta::sidecar_path(src);
            if side.exists() {
                fs::copy(&side, meta::sidecar_path(&target))?;
            }
            Ok(already)
        })();
        match res {
            Ok(true) => skipped += 1,
            Ok(false) => copied += 1,
            Err(e) => errors.push(format!("{}: {e}", src.display())),
        }
    }
    let _ = tx.send(Msg::CopyDone { copied, skipped, errors, dest: dest.to_path_buf() });
    ctx.request_repaint();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_keyword_toggles_reach_the_file_and_report_back() {
        let dir = std::env::temp_dir().join(format!("picker-writer-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (good, bad) = (dir.join("good.jpg"), dir.join("bad.jpg"));
        let jpeg = [0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02, 1, 2, 3, 0xFF, 0xD9];
        fs::write(&good, jpeg).unwrap();
        fs::write(&bad, b"not a jpeg").unwrap();

        let (msg_tx, msg_rx) = std::sync::mpsc::channel();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let cmd = |idx, path: &Path, on| WriteCmd::Keyword { generation: 7, idx, path: path.to_path_buf(), keyword: "pere".into(), on };
        // Queued before the writer starts, so they arrive as one batch: on, off, on.
        for on in [true, false, true] {
            cmd_tx.send(cmd(0, &good, on)).unwrap();
        }
        cmd_tx.send(cmd(1, &bad, true)).unwrap();
        drop(cmd_tx);
        start_writer(egui::Context::default(), msg_tx, cmd_rx).join().unwrap();

        let mut done: Vec<_> = msg_rx
            .try_iter()
            .filter_map(|m| match m {
                Msg::KeywordWritten { generation: 7, idx, on, count, result } => Some((idx, on, count, result.is_ok())),
                _ => None,
            })
            .collect();
        done.sort();
        assert_eq!(done, [(0, true, 3, true), (1, true, 1, false)]);
        assert_eq!(meta::read_meta_from_file(&good).unwrap().keywords, ["pere"]);
        assert_eq!(fs::read(&bad).unwrap(), b"not a jpeg");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn copy_keeps_same_named_photos_apart_and_brings_sidecars() {
        let dir = std::env::temp_dir().join(format!("picker-copy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (a, b, dest) = (dir.join("a"), dir.join("b"), dir.join("dest"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        // Two different photos with the same name and size, from different folders.
        fs::write(a.join("IMG_1.HEIC"), b"aaaa").unwrap();
        fs::write(b.join("IMG_1.HEIC"), b"bbbb").unwrap();
        filetime::set_file_mtime(b.join("IMG_1.HEIC"), filetime::FileTime::from_unix_time(1_000_000_000, 0)).unwrap();
        fs::write(meta::sidecar_path(&a.join("IMG_1.HEIC")), "rating").unwrap();
        let files = vec![(a.join("IMG_1.HEIC"), Some("2016-03".to_string())), (b.join("IMG_1.HEIC"), Some("2016-03".to_string()))];

        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = egui::Context::default();
        copy_files(&files, &dest, &ctx, &tx);
        let month = dest.join("2016-03");
        assert_eq!(fs::read(month.join("IMG_1.HEIC")).unwrap(), b"aaaa");
        assert_eq!(fs::read(month.join("IMG_1_1.HEIC")).unwrap(), b"bbbb");
        assert_eq!(fs::read_to_string(month.join("IMG_1.HEIC.xmp")).unwrap(), "rating");

        // Copying again finds both already there.
        copy_files(&files, &dest, &ctx, &tx);
        let done: Vec<(usize, usize)> = rx
            .try_iter()
            .filter_map(|m| match m {
                Msg::CopyDone { copied, skipped, .. } => Some((copied, skipped)),
                _ => None,
            })
            .collect();
        assert_eq!(done, [(2, 0), (0, 2)]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
