use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, FontId, Key as K, Pos2, Rect, Sense, Stroke, TextureHandle, Vec2, pos2, vec2,
};

use crate::meta::{self, CaptureTime, Meta};
use crate::workers::{self, Job, Key, Kind, Msg, Scheduler, WriteCmd};

pub const PICK: u8 = 4;
/// First-pass "roughly good" rating, to pick from later.
pub const SHORTLIST: u8 = 3;
pub const REJECT: u8 = 1;
/// Photos rated at least this are "picked" (copied by "Copy picked").
pub const PICKED_MIN: u8 = 4;

const AHEAD: usize = 6;
const BEHIND: usize = 3;
const THUMB_PX: u32 = 256;
const STRIP_H: f32 = 96.0;
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Per-month photo counts for the month bar.
#[derive(Default)]
struct MonthStats {
    total: usize,
    unrated: usize,
    picked: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Unrated,
    Shortlist,
    Picked,
    Rejected,
}

impl Filter {
    fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Unrated => "Unrated",
            Filter::Shortlist => "Shortlist",
            Filter::Picked => "Picked",
            Filter::Rejected => "Rejected",
        }
    }
    fn next(self) -> Self {
        match self {
            Filter::All => Filter::Unrated,
            Filter::Unrated => Filter::Shortlist,
            Filter::Shortlist => Filter::Picked,
            Filter::Picked => Filter::Rejected,
            Filter::Rejected => Filter::All,
        }
    }
    fn matches(self, rating: Option<u8>) -> bool {
        let r = rating.unwrap_or(0);
        match self {
            Filter::All => true,
            Filter::Unrated => r == 0,
            // Picks stay visible, so the second pass can work through the shortlist.
            Filter::Shortlist => r >= SHORTLIST,
            Filter::Picked => r >= PICKED_MIN,
            Filter::Rejected => r == REJECT,
        }
    }
}

struct Photo {
    path: PathBuf,
    name: String,
    /// `None` until the metadata pass has read it.
    rating: Option<u8>,
    meta: Option<Meta>,
    /// Capture time, from EXIF or the file's modification time. `None` until read.
    when: Option<CaptureTime>,
    /// Number of rating writes not yet confirmed.
    pending: u32,
    /// The last rating write failed, so the file doesn't have the shown rating.
    unsaved: bool,
    error: Option<String>,
}

/// An error notice shown over the photo until dismissed or timed out.
struct Snack {
    /// Errors with the same key are merged into one notice.
    key: &'static str,
    title: String,
    detail: String,
    count: u32,
    since: Instant,
}

const SNACK_SECS: u64 = 15;

struct Tex {
    tex: TextureHandle,
    target: [u32; 2],
}

pub struct PickerApp {
    ctx: egui::Context,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    sched: Arc<Scheduler>,
    write_tx: Option<Sender<WriteCmd>>,
    writer: Option<JoinHandle<()>>,

    generation: u64,
    root: Option<PathBuf>,
    scanning: bool,
    /// In scan order; indices into it are stable for the whole session.
    photos: Vec<Photo>,
    /// Display order: position → photo index. Path order until all dates are read.
    order: Vec<usize>,
    /// Photo index → display position.
    pos_of: Vec<usize>,
    /// Number of photos whose metadata (and date) has been read.
    meta_done: usize,
    /// Month filter: (year, month), or `None` for all.
    month: Option<(i32, u32)>,
    /// Photo index.
    cur: usize,
    /// +1 when moving forward, -1 backward (biases prefetching).
    dir: i8,

    previews: HashMap<usize, Tex>,
    fulls: HashMap<usize, Tex>,
    thumbs: HashMap<usize, TextureHandle>,
    preview_target: [u32; 2],
    /// Last requested state, to avoid rebuilding the job queue every frame.
    last_request: Option<(u64, usize, [u32; 2], bool, usize, usize, usize)>,

    filter: Filter,
    auto_advance: bool,
    show_info: bool,
    show_help: bool,
    zoom: bool,
    /// Pan offset in points (zoom mode), relative to image center.
    pan: Vec2,
    status: Option<(String, Instant)>,
    copy_progress: Option<(usize, usize)>,
    /// The window was hidden (minimized, occluded) on the previous frame.
    was_hidden: bool,
    snacks: Vec<Snack>,
    /// Where the photo is drawn, for placing error notices over it.
    view_rect: Rect,
}

impl PickerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, start: Option<PathBuf>) -> Self {
        let ctx = cc.egui_ctx.clone();
        apply_theme(&ctx);
        let (tx, rx) = mpsc::channel();
        let sched = Scheduler::start(ctx.clone(), tx.clone());
        let (write_tx, write_rx) = mpsc::channel();
        let writer = workers::start_writer(ctx.clone(), tx.clone(), write_rx);
        let mut app = PickerApp {
            ctx,
            tx,
            rx,
            sched,
            write_tx: Some(write_tx),
            writer: Some(writer),
            generation: 0,
            root: None,
            scanning: false,
            photos: Vec::new(),
            order: Vec::new(),
            pos_of: Vec::new(),
            meta_done: 0,
            month: None,
            cur: 0,
            dir: 1,
            previews: HashMap::new(),
            fulls: HashMap::new(),
            thumbs: HashMap::new(),
            preview_target: [0, 0],
            last_request: None,
            filter: Filter::All,
            auto_advance: true,
            show_info: true,
            show_help: false,
            zoom: false,
            pan: Vec2::ZERO,
            status: None,
            copy_progress: None,
            was_hidden: false,
            snacks: Vec::new(),
            view_rect: Rect::NOTHING,
        };
        if let Some(p) = start {
            app.open(p);
        }
        app
    }

    fn open(&mut self, root: PathBuf) {
        self.generation += 1;
        self.root = Some(root.clone());
        self.scanning = true;
        self.photos.clear();
        self.order.clear();
        self.pos_of.clear();
        self.meta_done = 0;
        self.month = None;
        self.previews.clear();
        self.fulls.clear();
        self.thumbs.clear();
        self.cur = 0;
        self.zoom = false;
        self.last_request = None;
        self.sched.set_jobs(Vec::new());
        workers::scan(self.generation, root, self.ctx.clone(), self.tx.clone());
    }

    fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some((s.into(), Instant::now()));
    }

    fn show_error(&mut self, key: &'static str, title: String, detail: String) {
        let now = Instant::now();
        if let Some(sn) = self.snacks.iter_mut().find(|sn| sn.key == key) {
            sn.count += 1;
            (sn.title, sn.detail, sn.since) = (title, detail, now);
        } else {
            self.snacks.push(Snack { key, title, detail, count: 1, since: now });
        }
    }

    // -----------------------------------------------------------------------
    // Messages from workers

    fn drain_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Scanned { generation, root, paths } if generation == self.generation => {
                    self.scanning = false;
                    self.photos = paths
                        .into_iter()
                        .map(|path| Photo {
                            name: path
                                .strip_prefix(&root)
                                .ok()
                                .filter(|p| !p.as_os_str().is_empty())
                                .unwrap_or(&path)
                                .to_string_lossy()
                                .into_owned(),
                            path,
                            rating: None,
                            meta: None,
                            when: None,
                            pending: 0,
                            unsaved: false,
                            error: None,
                        })
                        .collect();
                    self.order = (0..self.photos.len()).collect();
                    self.pos_of = self.order.clone();
                    if self.photos.is_empty() {
                        self.set_status("No JPEG files found in this folder");
                    }
                }
                Msg::Meta { generation, idx, meta } if generation == self.generation => {
                    if let Some(p) = self.photos.get_mut(idx) {
                        // Don't overwrite a rating the user already set this session.
                        if p.pending == 0 && p.rating.is_none() {
                            p.rating = Some(meta.rating.unwrap_or(0));
                        }
                        p.when = meta::capture_time(&meta);
                        p.meta = Some(meta);
                        self.meta_done += 1;
                        if self.meta_done == self.photos.len() {
                            self.sort_by_date();
                        }
                    }
                }
                Msg::Image { key, target, epoch, result } => {
                    self.sched.finished(&key);
                    if key.generation != self.generation {
                        continue;
                    }
                    if result.is_ok() && epoch != self.sched.hidden_epoch.load(Ordering::SeqCst) {
                        // The upload may have been dropped on a hidden frame: decode again.
                        self.last_request = None;
                        continue;
                    }
                    match result {
                        Ok(tex) => match key.kind {
                            Kind::Preview => {
                                self.previews.insert(key.idx, Tex { tex, target });
                            }
                            Kind::Full => {
                                self.fulls.insert(key.idx, Tex { tex, target });
                            }
                            Kind::Thumb => {
                                self.thumbs.insert(key.idx, tex);
                            }
                        },
                        Err(e) => {
                            if let Some(p) = self.photos.get_mut(key.idx) {
                                if key.kind != Kind::Thumb {
                                    p.error = Some(format!("Cannot decode: {e}"));
                                }
                            }
                        }
                    }
                    self.last_request = None;
                }
                Msg::Written { generation, idx, rating, count, result } if generation == self.generation => {
                    if let Some(p) = self.photos.get_mut(idx) {
                        p.pending = p.pending.saturating_sub(count);
                        p.unsaved = result.is_err();
                        match result {
                            Ok(()) => p.error = None,
                            Err(e) => {
                                p.error = Some(format!("Failed to write ★{rating} to {}: {e}", p.name));
                                let title = format!("Rating not saved: {}", p.name);
                                self.show_error("write", title, e);
                            }
                        }
                    }
                }
                Msg::CopyProgress { done, total } => self.copy_progress = Some((done, total)),
                Msg::CopyDone { copied, skipped, errors, dest } => {
                    self.copy_progress = None;
                    let mut s = format!("Copied {copied} photo(s) to {}", dest.display());
                    if skipped > 0 {
                        s += &format!(", {skipped} already there");
                    }
                    if !errors.is_empty() {
                        s += &format!(", {} failed", errors.len());
                        let title = format!("{} photo(s) could not be copied", errors.len());
                        self.show_error("copy", title, errors[0].clone());
                    }
                    self.set_status(s);
                }
                _ => {} // Stale message from a previously opened folder.
            }
        }
    }

    // -----------------------------------------------------------------------
    // Navigation & rating

    /// Once all dates are read: order by capture time (path order breaks ties, no date last).
    /// The current photo stays current.
    fn sort_by_date(&mut self) {
        let photos = &self.photos;
        self.order.sort_by(|&a, &b| {
            let key = |i: usize| photos[i].when.as_ref().map(|w| w.key.as_str());
            let (ka, kb) = (key(a), key(b));
            ka.is_none().cmp(&kb.is_none()).then(ka.cmp(&kb))
        });
        for (pos, &i) in self.order.iter().enumerate() {
            self.pos_of[i] = pos;
        }
        self.last_request = None;
    }

    fn dates_ready(&self) -> bool {
        !self.photos.is_empty() && self.meta_done == self.photos.len()
    }

    fn in_month(&self, i: usize) -> bool {
        match self.month {
            None => true,
            Some((y, m)) => self.photos[i].when.as_ref().is_some_and(|w| w.year == y && w.month == m),
        }
    }

    /// Whether photo `i` passes both the rating filter and the month filter.
    fn shows(&self, i: usize) -> bool {
        self.filter.matches(self.photos[i].rating) && self.in_month(i)
    }

    /// Visible photo indices, in display order.
    fn visible_indices(&self) -> Vec<usize> {
        self.order.iter().copied().filter(|&i| self.shows(i)).collect()
    }

    /// Months that have photos, in date order, with their counts.
    fn months(&self) -> BTreeMap<(i32, u32), MonthStats> {
        let mut out: BTreeMap<(i32, u32), MonthStats> = BTreeMap::new();
        for p in &self.photos {
            let Some(w) = &p.when else { continue };
            let st = out.entry((w.year, w.month)).or_default();
            st.total += 1;
            let r = p.rating.unwrap_or(0);
            st.unrated += usize::from(r == 0);
            st.picked += usize::from(r >= PICKED_MIN);
        }
        out
    }

    fn set_month(&mut self, month: Option<(i32, u32)>) {
        self.month = month;
        if self.photos.is_empty() {
            return;
        }
        // Land on the first matching photo of the month.
        if let Some(&i) = self.visible_indices().first() {
            if month.is_some() || !self.shows(self.cur) {
                self.go_to(i);
            }
        }
    }

    /// Previous / next month that has photos (`delta` is -1 or 1).
    fn step_month(&mut self, delta: i32) {
        if !self.dates_ready() {
            return;
        }
        let months: Vec<(i32, u32)> = self.months().into_keys().collect();
        let Some(&last) = months.last() else { return };
        let next = match self.month.and_then(|m| months.iter().position(|&x| x == m)) {
            None => if delta > 0 { months[0] } else { last },
            Some(p) => months[(p as i32 + delta).clamp(0, months.len() as i32 - 1) as usize],
        };
        self.set_month(Some(next));
    }

    fn step(&mut self, delta: isize) {
        if self.photos.is_empty() {
            return;
        }
        let n = self.order.len() as isize;
        let mut pos = self.pos_of[self.cur] as isize;
        let mut remaining = delta.abs();
        let s = delta.signum();
        let mut last_ok = self.cur;
        while remaining > 0 {
            pos += s;
            if pos < 0 || pos >= n {
                break;
            }
            let i = self.order[pos as usize];
            if self.shows(i) {
                last_ok = i;
                remaining -= 1;
            }
        }
        self.go_to(last_ok);
        if delta != 0 {
            self.dir = s as i8;
        }
    }

    fn go_to(&mut self, idx: usize) {
        if idx != self.cur {
            self.cur = idx;
            self.zoom = false;
            self.pan = Vec2::ZERO;
        }
    }

    /// After a filter change, make sure the current photo is visible.
    fn ensure_visible(&mut self) {
        if self.photos.is_empty() || self.shows(self.cur) {
            return;
        }
        let vis = self.visible_indices();
        let cur_pos = self.pos_of[self.cur];
        if let Some(&i) = vis.iter().find(|&&i| self.pos_of[i] > cur_pos).or(vis.last()) {
            self.go_to(i);
        }
    }

    fn rate(&mut self, rating: u8) {
        let generation = self.generation;
        let idx = self.cur;
        let Some(p) = self.photos.get_mut(idx) else { return };
        p.rating = Some(rating);
        p.pending += 1;
        if let Some(tx) = &self.write_tx {
            let _ = tx.send(WriteCmd::Rate { generation, idx, path: p.path.clone(), rating });
        }
        if self.auto_advance {
            let before = self.cur;
            self.step(1);
            if self.cur == before {
                self.ensure_visible();
            }
        } else {
            self.ensure_visible();
        }
    }

    /// P picks; on an already picked photo it unpicks (★1) instead.
    fn toggle_pick(&mut self) {
        let Some(p) = self.photos.get(self.cur) else { return };
        let picked = p.rating.unwrap_or(0) >= PICKED_MIN;
        self.rate(if picked { REJECT } else { PICK });
    }

    /// S shortlists (★3); on an already shortlisted photo it rejects (★1) instead.
    fn toggle_shortlist(&mut self) {
        let Some(p) = self.photos.get(self.cur) else { return };
        let shortlisted = p.rating == Some(SHORTLIST);
        self.rate(if shortlisted { REJECT } else { SHORTLIST });
    }

    fn copy_picked(&mut self) {
        let files: Vec<PathBuf> = self
            .photos
            .iter()
            .filter(|p| p.rating.unwrap_or(0) >= PICKED_MIN)
            .map(|p| p.path.clone())
            .collect();
        if files.is_empty() {
            self.set_status("Nothing picked yet — press P to pick a photo");
            return;
        }
        let Some(dest) = rfd::FileDialog::new()
            .set_title(format!("Copy {} picked photo(s) to…", files.len()))
            .pick_folder()
        else {
            return;
        };
        if let Some(tx) = &self.write_tx {
            self.copy_progress = Some((0, files.len()));
            let _ = tx.send(WriteCmd::Copy { files, dest });
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let (events, cmd) = ctx.input(|i| (i.events.clone(), i.modifiers.command));
        for ev in events {
            let egui::Event::Key { key, pressed: true, modifiers, .. } = ev else { continue };
            if cmd {
                if key == K::O {
                    self.pick_folder();
                }
                continue;
            }
            match key {
                K::OpenBracket => self.step_month(-1),
                K::CloseBracket => self.step_month(1),
                K::ArrowLeft if modifiers.shift => self.step_month(-1),
                K::ArrowRight if modifiers.shift => self.step_month(1),
                K::M => self.set_month(None),
                K::ArrowRight | K::ArrowDown | K::Space => self.step(1),
                K::ArrowLeft | K::ArrowUp | K::Backspace => self.step(-1),
                K::PageDown => self.step(10),
                K::PageUp => self.step(-10),
                K::Home => self.step(-(self.photos.len() as isize)),
                K::End => self.step(self.photos.len() as isize),
                K::P => self.toggle_pick(),
                K::S => self.toggle_shortlist(),
                K::X | K::U => self.rate(REJECT),
                K::Num0 => self.rate(0),
                K::Num1 => self.rate(1),
                K::Num2 => self.rate(2),
                K::Num3 => self.rate(3),
                K::Num4 => self.rate(4),
                K::Num5 => self.rate(5),
                K::Z | K::Enter => {
                    self.zoom = !self.zoom;
                    self.pan = Vec2::ZERO;
                }
                K::I => self.show_info = !self.show_info,
                K::A => self.auto_advance = !self.auto_advance,
                K::F => {
                    self.filter = self.filter.next();
                    self.ensure_visible();
                }
                K::H | K::Questionmark | K::F1 => self.show_help = !self.show_help,
                K::Escape => {
                    self.zoom = false;
                    self.show_help = false;
                }
                _ => {}
            }
        }
    }

    fn pick_folder(&mut self) {
        let mut dlg = rfd::FileDialog::new().set_title("Open photo folder (e.g. the SD card)");
        if let Some(r) = &self.root {
            dlg = dlg.set_directory(r);
        }
        if let Some(p) = dlg.pick_folder() {
            self.open(p);
        }
    }

    // -----------------------------------------------------------------------
    // Prefetching

    /// `strip_range` is a range of positions in [`Self::visible_indices`].
    fn request_images(&mut self, strip_range: (usize, usize)) {
        let n = self.photos.len();
        if n == 0 || self.preview_target == [0, 0] {
            return;
        }
        let vis = self.visible_indices();
        let strip = &vis[strip_range.0.min(vis.len())..strip_range.1.min(vis.len())];
        let ends = |s: &[usize]| (s.first().copied().unwrap_or(usize::MAX), s.last().copied().unwrap_or(usize::MAX));
        let (strip_first, strip_last) = ends(strip);
        let state = (self.generation, self.cur, self.preview_target, self.zoom, strip_first, strip_last, vis.len());
        if self.last_request == Some(state) {
            return;
        }
        self.last_request = Some(state);

        // Neighbours in the visible (filtered, display-ordered) list.
        let cur_vis = vis.iter().position(|&i| i == self.cur);
        let near = |d_fwd: usize, d_back: usize| -> Vec<usize> {
            let mut out = vec![self.cur];
            let Some(c) = cur_vis else { return out };
            for d in 1..=d_fwd.max(d_back) {
                if d <= d_fwd && c + d < vis.len() {
                    out.push(vis[c + d]);
                }
                if d <= d_back && c >= d {
                    out.push(vis[c - d]);
                }
            }
            out
        };
        // Order: current, then mostly in the direction of travel.
        let (fwd, back) = if self.dir >= 0 { (AHEAD, BEHIND) } else { (BEHIND, AHEAD) };
        let order = near(fwd, back);
        let keep: HashSet<usize> = near(AHEAD.max(BEHIND) + 2, AHEAD.max(BEHIND) + 2).into_iter().collect();
        let thumb_keep: HashSet<usize> = vis
            [strip_range.0.saturating_sub(100).min(vis.len())..(strip_range.1 + 100).min(vis.len())]
            .iter()
            .copied()
            .chain(keep.iter().copied())
            .collect();

        let target = self.preview_target;
        let generation = self.generation;
        let mut jobs = Vec::new();
        let add = |idx: usize, kind: Kind, target: [u32; 2], jobs: &mut Vec<Job>| {
            jobs.push(Job { key: Key { generation, idx, kind }, path: self.photos[idx].path.clone(), target });
        };

        let thumb_missing = |i: usize| !self.thumbs.contains_key(&i);
        // The current photo's thumbnail is a near-instant placeholder.
        if thumb_missing(self.cur) && !self.previews.contains_key(&self.cur) {
            add(self.cur, Kind::Thumb, [THUMB_PX, THUMB_PX], &mut jobs);
        }
        let preview_ok = |i: &usize| self.previews.get(i).is_some_and(|t| t.target == target);
        if self.zoom && !self.fulls.contains_key(&self.cur) {
            if !preview_ok(&self.cur) {
                add(self.cur, Kind::Preview, target, &mut jobs);
            }
            add(self.cur, Kind::Full, [u32::MAX, u32::MAX], &mut jobs);
        }
        for &i in &order {
            if !preview_ok(&i) {
                add(i, Kind::Preview, target, &mut jobs);
            }
        }
        for &i in strip {
            if thumb_missing(i) {
                add(i, Kind::Thumb, [THUMB_PX, THUMB_PX], &mut jobs);
            }
        }
        jobs.dedup_by_key(|j| j.key);
        self.sched.set_jobs(jobs);

        // Evict textures far from the current position.
        let cur = self.cur;
        self.previews.retain(|i, _| keep.contains(i));
        self.fulls.retain(|&i, _| i == cur);
        self.thumbs.retain(|i, _| thumb_keep.contains(i));
    }

    // -----------------------------------------------------------------------
    // Drawing

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            if ui.button("Open").on_hover_text("Open a folder (Ctrl/Cmd+O)").clicked() {
                self.pick_folder();
            }
            let picked = self.photos.iter().filter(|p| p.rating.unwrap_or(0) >= PICKED_MIN).count();
            let copy_btn = ui.add_enabled(
                picked > 0 && self.copy_progress.is_none(),
                egui::Button::new(format!("Copy {picked} picked")),
            );
            if copy_btn.on_hover_text("Copy all photos rated ★4 or more to a folder").clicked() {
                self.copy_picked();
            }
            divider(ui);
            for f in [Filter::All, Filter::Unrated, Filter::Shortlist, Filter::Picked, Filter::Rejected] {
                let hint = if f == Filter::Shortlist { "Shortlisted and picked photos (★3+) — F to cycle" } else { "Filter (F)" };
                if ui.selectable_label(self.filter == f, f.label()).on_hover_text(hint).clicked() {
                    self.filter = f;
                    self.ensure_visible();
                }
            }
            divider(ui);
            if ui
                .selectable_label(self.auto_advance, "Auto-advance")
                .on_hover_text("Go to the next photo after rating (A)")
                .clicked()
            {
                self.auto_advance = !self.auto_advance;
            }
            if ui.selectable_label(self.show_help, "?").on_hover_text("Keyboard shortcuts (H)").clicked() {
                self.show_help = !self.show_help;
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let pending: u32 = self.photos.iter().map(|p| p.pending).sum();
                let unsaved = self.photos.iter().filter(|p| p.unsaved).count();
                if unsaved > 0 {
                    let first = self.photos.iter().find(|p| p.unsaved).and_then(|p| p.error.clone()).unwrap_or_default();
                    ui.label(egui::RichText::new(format!("⚠ {unsaved} rating(s) NOT saved")).color(BRIGHT).strong())
                        .on_hover_text(first);
                    divider(ui);
                }
                if let Some((done, total)) = self.copy_progress {
                    ui.label(format!("Copying {done}/{total}"));
                    ui.spinner();
                } else if pending > 0 {
                    ui.label(format!("Saving {pending}"));
                    ui.spinner();
                } else if !self.photos.is_empty() {
                    ui.label(egui::RichText::new("Saved").color(MUTED));
                }
                divider(ui);
                if !self.photos.is_empty() {
                    // Counts follow the month filter (but not the rating filter).
                    let ratings: Vec<u8> = (0..self.photos.len())
                        .filter(|&i| self.in_month(i))
                        .map(|i| self.photos[i].rating.unwrap_or(0))
                        .collect();
                    let count = |f: fn(u8) -> bool| ratings.iter().filter(|&&r| f(r)).count();
                    let photos_label = match self.month {
                        Some(m) => format!("photos in {}", month_name(m)),
                        None => "photos".to_string(),
                    };
                    for (n, label) in [
                        (count(|r| r == 0), "unrated"),
                        (count(|r| r == REJECT), "rejected"),
                        (count(|r| r == SHORTLIST), "shortlisted"),
                        (count(|r| r >= PICKED_MIN), "picked"),
                        (ratings.len(), photos_label.as_str()),
                    ] {
                        ui.label(egui::RichText::new(label).color(MUTED));
                        ui.label(egui::RichText::new(n.to_string()).color(BRIGHT));
                        ui.add_space(6.0);
                    }
                }
                if let Some(root) = &self.root {
                    ui.add(egui::Label::new(egui::RichText::new(root.display().to_string()).color(FAINT)).truncate());
                }
            });
        });
    }

    /// One chip per month that has photos; months with nothing left to rate are dimmed.
    fn month_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            if !self.dates_ready() {
                ui.spinner();
                ui.label(egui::RichText::new(format!("Reading dates… {}/{}", self.meta_done, self.photos.len())).color(MUTED));
                return;
            }
            let months = self.months();
            let multi_year = months.keys().next().map(|k| k.0) != months.keys().next_back().map(|k| k.0);
            let mut chosen = None;
            if ui.selectable_label(self.month.is_none(), "All months").on_hover_text("Show all months (M)").clicked() {
                chosen = Some(None);
            }
            divider(ui);
            for (&(y, m), st) in &months {
                let name = MONTHS[(m - 1) as usize];
                let label = if multi_year { format!("{name} ’{:02} ", y.rem_euclid(100)) } else { format!("{name} ") };
                let mut text = egui::text::LayoutJob::default();
                let done = st.unrated == 0;
                let fmt = |color| egui::TextFormat { color, font_id: FontId::proportional(13.0), ..Default::default() };
                text.append(&label, 0.0, fmt(if done { FAINT } else { TEXT }));
                text.append(&st.total.to_string(), 0.0, fmt(if done { FAINT } else { MUTED }));
                let hover = format!("{} unrated · {} picked   ([ / ] previous / next month)", st.unrated, st.picked);
                if ui.selectable_label(self.month == Some((y, m)), text).on_hover_text(hover).clicked() {
                    chosen = Some(Some((y, m)));
                }
            }
            if let Some(m) = chosen {
                self.set_month(m);
            }
        });
    }

    /// Draws the filmstrip, returns the range of visible positions it shows (plus a margin).
    fn filmstrip(&mut self, ui: &mut egui::Ui) -> (usize, usize) {
        let rect = ui.available_rect_before_wrap();
        let vis = self.visible_indices();
        if vis.is_empty() {
            return (0, 0);
        }
        let item_w = STRIP_H * 1.4;
        let slots = ((rect.width() / item_w).floor() as usize).max(1);
        let pos = vis.iter().position(|&i| i == self.cur).unwrap_or(0);
        let first = pos.saturating_sub(slots / 2).min(vis.len().saturating_sub(slots));
        let shown = &vis[first..(first + slots).min(vis.len())];
        let painter = ui.painter_at(rect);
        let x0 = rect.center().x - (shown.len() as f32 * item_w) / 2.0;
        let mut clicked = None;
        for (k, &idx) in shown.iter().enumerate() {
            let cell = Rect::from_min_size(pos2(x0 + k as f32 * item_w, rect.top()), vec2(item_w, rect.height()))
                .shrink(3.0);
            let resp = ui.interact(cell, ui.id().with(("thumb", idx)), Sense::click());
            if resp.clicked() {
                clicked = Some(idx);
            }
            let is_cur = idx == self.cur;
            let rating = self.photos[idx].rating.unwrap_or(0);
            let img_rect = Rect::from_min_max(cell.min, pos2(cell.max.x, cell.max.y - 14.0));
            // Dim everything but the current photo; rejects fade further.
            let tint = match (is_cur, resp.hovered(), rating) {
                (true, ..) => Color32::WHITE,
                (_, true, _) => Color32::from_gray(220),
                (_, _, REJECT) => Color32::from_gray(70),
                _ => Color32::from_gray(150),
            };
            let shown_rect = if let Some(t) = self.thumbs.get(&idx) {
                let r = fit_rect(t.size_vec2(), img_rect);
                painter.image(t.id(), r, unit_uv(), tint);
                r
            } else {
                let r = img_rect.shrink(6.0);
                painter.rect_filled(r, 2.0, SURFACE);
                r
            };
            painter.text(
                pos2(cell.center().x, cell.max.y - 6.0),
                Align2::CENTER_CENTER,
                stars_text(rating),
                FontId::proportional(9.0),
                if is_cur { BRIGHT } else { rating_color(rating).gamma_multiply(0.8) },
            );
            if is_cur {
                painter.rect_stroke(shown_rect, 1.0, Stroke::new(1.5, BRIGHT), egui::StrokeKind::Outside);
            }
        }
        if let Some(i) = clicked {
            self.dir = if self.pos_of[i] >= self.pos_of[self.cur] { 1 } else { -1 };
            self.go_to(i);
        }
        (first.saturating_sub(4), (first + shown.len() + 5).min(vis.len()))
    }

    fn main_view(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let ppp = ui.ctx().pixels_per_point();

        // Decode previews at ~80% of the physical view size: this lets the
        // decoder use 1/2 or 1/4 DCT scaling far more often, at no visible cost.
        let phys = [(rect.width() * ppp * 0.8) as u32, (rect.height() * ppp * 0.8) as u32];
        let bucket = |v: u32| v.div_ceil(128) * 128;
        let want = [bucket(phys[0]), bucket(phys[1])];
        if want[0] > self.preview_target[0] || want[1] > self.preview_target[1] {
            self.preview_target = [want[0].max(self.preview_target[0]), want[1].max(self.preview_target[1])];
        }

        self.view_rect = rect;
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, CANVAS);

        if self.photos.is_empty() {
            let msg = if self.scanning {
                "Scanning for photos…"
            } else if self.root.is_some() {
                "No JPEG photos here. Open another folder with Ctrl/Cmd+O."
            } else {
                "Open a folder (Ctrl/Cmd+O) or drop one on this window to start picking."
            };
            painter.text(rect.center(), Align2::CENTER_CENTER, msg, FontId::proportional(15.0), MUTED);
            return;
        }
        if !self.shows(self.cur) {
            let msg = match self.month {
                Some(m) => format!("No photos match “{}” in {} (press F or M to change)", self.filter.label(), month_name(m)),
                None => format!("No photos match “{}” (press F to change)", self.filter.label()),
            };
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                msg,
                FontId::proportional(15.0),
                MUTED,
            );
            return;
        }

        let resp = ui.interact(rect, ui.id().with("main"), Sense::click_and_drag());
        let cur = self.cur;
        let full = self.fulls.get(&cur);
        let preview = self.previews.get(&cur);

        if self.zoom {
            // 1:1 physical pixels of the full-resolution image.
            let size = self.full_size(cur, rect.size() * ppp) / ppp;
            if resp.dragged() {
                self.pan += resp.drag_delta();
            }
            let max_pan = ((size - rect.size()) / 2.0).max(Vec2::ZERO);
            self.pan = self.pan.clamp(-max_pan, max_pan);
            let img_rect = Rect::from_center_size(rect.center() + self.pan, size);
            if let Some(t) = full.map(|t| &t.tex).or(preview.map(|t| &t.tex)).or(self.thumbs.get(&cur)) {
                painter.image(t.id(), img_rect, unit_uv(), Color32::WHITE);
            }
            if full.is_none() {
                painter.text(rect.left_bottom() + vec2(12.0, -12.0), Align2::LEFT_BOTTOM, "Loading full resolution…", FontId::proportional(12.0), MUTED);
            }
            if resp.clicked() {
                self.zoom = false;
            }
            ui.ctx().set_cursor_icon(if resp.dragged() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
        } else {
            let tex = preview.map(|t| &t.tex).or(self.thumbs.get(&cur));
            if let Some(t) = tex {
                let r = fit_rect(t.size_vec2(), rect);
                painter.image(t.id(), r, unit_uv(), Color32::WHITE);
                if resp.clicked() {
                    // Zoom so the clicked spot stays under the cursor.
                    if let Some(p) = resp.interact_pointer_pos() {
                        let rel = (p - r.min) / r.size();
                        let size = self.full_size(cur, t.size_vec2()) / ppp;
                        self.pan = p - rect.center() + size * (vec2(0.5, 0.5) - rel);
                    }
                    self.zoom = true;
                }
            } else if let Some(err) = &self.photos[cur].error {
                painter.text(rect.center(), Align2::CENTER_CENTER, err, FontId::proportional(14.0), TEXT);
            } else {
                painter.text(rect.center(), Align2::CENTER_CENTER, "Loading…", FontId::proportional(14.0), MUTED);
            }
        }

        self.overlay(ui, rect);
    }

    /// Full-resolution size in pixels (known from the header before decoding).
    fn full_size(&self, idx: usize, fallback: Vec2) -> Vec2 {
        if let Some(t) = self.fulls.get(&idx) {
            return t.tex.size_vec2();
        }
        match self.photos[idx].meta.as_ref().and_then(|m| m.size) {
            Some((w, h)) => {
                let max = self.ctx.input(|i| i.max_texture_side) as f32;
                let v = vec2(w as f32, h as f32);
                // The full decode is capped at the GPU texture size.
                if v.max_elem() > max { v * (max / v.max_elem()) } else { v }
            }
            None => fallback,
        }
    }

    fn overlay(&self, ui: &egui::Ui, rect: Rect) {
        let painter = ui.painter_at(rect);
        let p = &self.photos[self.cur];
        let rating = p.rating.unwrap_or(0);

        // Big rating badge, top-left.
        let badge = match rating {
            r if r >= PICKED_MIN => format!("{}   PICKED", stars_text(r)),
            SHORTLIST => format!("{}   SHORTLISTED", stars_text(SHORTLIST)),
            REJECT => format!("{}   REJECTED", stars_text(REJECT)),
            r => stars_text(r),
        };
        let galley = painter.layout_no_wrap(badge, FontId::proportional(15.0), rating_color(rating));
        let bpos = rect.left_top() + vec2(20.0, 18.0);
        painter.rect_filled(Rect::from_min_size(bpos - vec2(10.0, 6.0), galley.size() + vec2(20.0, 12.0)), 4.0, Color32::from_black_alpha(150));
        painter.galley(bpos, galley, Color32::WHITE);

        if self.show_info {
            let vis = self.visible_indices();
            let k = vis.iter().position(|&i| i == self.cur).map_or(0, |k| k + 1);
            let mut lines = vec![format!("{}   ({k}/{})", p.name, vis.len())];
            if let Some(m) = &p.meta {
                let exposure: Vec<String> = [
                    m.focal.clone(),
                    m.aperture.clone(),
                    m.exposure.clone(),
                    m.iso.map(|i| format!("ISO {i}")),
                ]
                .into_iter()
                .flatten()
                .collect();
                if !exposure.is_empty() {
                    lines.push(exposure.join("   "));
                }
                let gear: Vec<String> = [m.camera.clone(), m.lens.clone()].into_iter().flatten().collect();
                if !gear.is_empty() {
                    lines.push(gear.join(" · "));
                }
            }
            match &p.when {
                Some(w) if w.exif => lines.push(w.label.clone()),
                Some(w) => lines.push(format!("{}  (file date, no EXIF date)", w.label)),
                None => {}
            }
            if p.pending > 0 {
                lines.push("saving rating…".into());
            }
            if let Some(e) = &p.error {
                lines.push(e.clone());
            }
            let mut job = egui::text::LayoutJob::default();
            for (k, line) in lines.iter().enumerate() {
                let (size, color) = if k == 0 { (13.0, BRIGHT) } else { (12.0, Color32::from_gray(165)) };
                let text = if k + 1 < lines.len() { format!("{line}\n") } else { line.clone() };
                job.append(&text, 0.0, egui::TextFormat { font_id: FontId::proportional(size), color, line_height: Some(size + 7.0), ..Default::default() });
            }
            job.wrap.max_width = 600.0;
            let galley = painter.layout_job(job);
            let pos = rect.right_top() + vec2(-20.0 - galley.size().x, 18.0);
            painter.rect_filled(Rect::from_min_size(pos - vec2(12.0, 8.0), galley.size() + vec2(24.0, 16.0)), 4.0, Color32::from_black_alpha(150));
            painter.galley(pos, galley, Color32::WHITE);
        }

        if self.zoom {
            painter.text(rect.center_bottom() + vec2(0.0, -12.0), Align2::CENTER_BOTTOM, "100%  ·  drag to pan  ·  Z or click to exit", FontId::proportional(12.0), Color32::from_gray(190));
        }
    }

    /// Error notices, stacked at the bottom of the photo where they can't be missed.
    fn snackbars(&mut self, ctx: &egui::Context) {
        self.snacks.retain(|sn| sn.since.elapsed() < Duration::from_secs(SNACK_SECS));
        if self.snacks.is_empty() {
            return;
        }
        let ink = Color32::from_gray(18);
        let rect = if self.view_rect.is_positive() { self.view_rect } else { ctx.content_rect() };
        let mut dismiss = None;
        let mut hovered = false;
        egui::Area::new(egui::Id::new("snackbars"))
            .order(egui::Order::Foreground)
            .pivot(Align2::CENTER_BOTTOM)
            .fixed_pos(rect.center_bottom() - vec2(0.0, 24.0))
            .interactable(true)
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                for (i, sn) in self.snacks.iter().enumerate() {
                    let resp = egui::Frame::NONE
                        .fill(Color32::from_gray(240))
                        .stroke(Stroke::new(1.0, Color32::from_gray(10)))
                        .corner_radius(6)
                        .inner_margin(egui::Margin { left: 16, right: 12, top: 12, bottom: 12 })
                        .shadow(egui::Shadow { offset: [0, 6], blur: 24, spread: 0, color: Color32::from_black_alpha(160) })
                        .show(ui, |ui| {
                            ui.set_min_width(460.0);
                            ui.set_max_width(620.0);
                            ui.horizontal_top(|ui| {
                                ui.label(egui::RichText::new("⚠").size(24.0).color(ink));
                                ui.add_space(4.0);
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    let more = if sn.count > 1 { format!("  (+{} more)", sn.count - 1) } else { String::new() };
                                    ui.label(egui::RichText::new(format!("{}{more}", sn.title)).size(15.0).strong().color(ink));
                                    ui.label(egui::RichText::new(&sn.detail).size(13.0).color(Color32::from_gray(70)));
                                });
                                let close = ui
                                    .with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                        ui.add(egui::Label::new(egui::RichText::new("×").size(20.0).color(Color32::from_gray(90))).sense(Sense::click()))
                                    })
                                    .inner
                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                    .on_hover_text("Dismiss");
                                if close.clicked() {
                                    dismiss = Some(i);
                                }
                            });
                        })
                        .response;
                    hovered |= resp.contains_pointer();
                }
            });
        if let Some(i) = dismiss {
            self.snacks.remove(i);
        }
        if hovered {
            // Keep notices up while the pointer is on them.
            let now = Instant::now();
            self.snacks.iter_mut().for_each(|sn| sn.since = now);
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_help;
        egui::Window::new("Keyboard shortcuts")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Grid::new("keys").num_columns(2).spacing([24.0, 6.0]).show(ui, |ui| {
                    for (k, d) in [
                        ("S", "Shortlist (rough pick) → ★★★ (3 stars); again → ★"),
                        ("P", "Pick → ★★★★ (4 stars); again to unpick → ★"),
                        ("X  or  U", "Unpick / reject → ★ (1 star)"),
                        ("1 … 5", "Set 1–5 stars"),
                        ("0", "Clear rating"),
                        ("→  ↓  Space", "Next photo"),
                        ("←  ↑  Backspace", "Previous photo"),
                        ("PgUp / PgDn", "Jump 10"),
                        ("Home / End", "First / last"),
                        ("Z  Enter  click", "Toggle 100% zoom (drag to pan)"),
                        ("F", "Cycle filter: All / Unrated / Shortlist / Picked / Rejected"),
                        ("[  ]   Shift+←  Shift+→", "Previous / next month"),
                        ("M", "All months"),
                        ("A", "Toggle auto-advance after rating"),
                        ("I", "Toggle photo info"),
                        ("Ctrl/Cmd + O", "Open folder"),
                        ("H  ?  F1", "This help"),
                    ] {
                        ui.label(egui::RichText::new(k).monospace().color(BRIGHT));
                        ui.label(egui::RichText::new(d).color(TEXT));
                        ui.end_row();
                    }
                });
                ui.add_space(12.0);
                ui.small("Ratings are written to the JPEG immediately (EXIF Rating + XMP xmp:Rating),\nso Lightroom, Bridge, Windows Explorer and others see them.\nOn macOS, picked photos also get a \"Picked\" Finder tag.");
            });
        self.show_help = open;
    }
}

impl eframe::App for PickerApp {
    /// Runs on every frame, unlike `ui`, which is skipped while the window is hidden.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let hidden = ctx.input(|i| i.viewport().visible() == Some(false));
        if hidden || self.was_hidden {
            self.sched.hidden_epoch.fetch_add(1, Ordering::SeqCst);
        }
        self.was_hidden = hidden;
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_messages();

        // Folder dropped on the window.
        let dropped = ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone()));
        if let Some(p) = dropped {
            self.open(if p.is_file() { p.parent().map(|p| p.to_path_buf()).unwrap_or(p) } else { p });
        }

        if !self.show_help || ctx.input(|i| i.key_pressed(K::Escape)) {
            self.handle_keys(&ctx);
        } else if ctx.input(|i| i.key_pressed(K::H) || i.key_pressed(K::F1)) {
            self.show_help = false;
        }

        egui::Panel::top("top")
            .exact_size(44.0)
            .frame(egui::Frame::NONE.fill(PANEL).inner_margin(egui::Margin::symmetric(12, 0)))
            .show(ui, |ui| {
                self.top_bar(ui);
            });
        if !self.photos.is_empty() {
            egui::Panel::top("months")
                .exact_size(34.0)
                .frame(egui::Frame::NONE.fill(CANVAS).inner_margin(egui::Margin::symmetric(12, 0)))
                .show(ui, |ui| {
                    self.month_bar(ui);
                });
        }
        if let Some((s, t)) = &self.status {
            if t.elapsed() < Duration::from_secs(8) {
                let s = s.clone();
                egui::Panel::bottom("status")
                    .frame(egui::Frame::NONE.fill(PANEL).inner_margin(egui::Margin::symmetric(14, 6)))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(s).color(TEXT));
                    });
                ctx.request_repaint_after(Duration::from_secs(1));
            } else {
                self.status = None;
            }
        }
        let mut strip_range = (0, 0);
        egui::Panel::bottom("strip")
            .exact_size(STRIP_H)
            .frame(egui::Frame::NONE.fill(PANEL).inner_margin(egui::Margin::symmetric(0, 6)))
            .show(ui, |ui| {
                strip_range = self.filmstrip(ui);
            });
        egui::CentralPanel::no_frame().show(ui, |ui| {
            self.main_view(ui);
        });

        self.snackbars(&ctx);
        self.help_window(&ctx);
        self.request_images(strip_range);
    }

    fn on_exit(&mut self) {
        // Let pending rating writes finish before quitting.
        self.write_tx = None;
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

fn unit_uv() -> Rect {
    Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0))
}

fn fit_rect(size: Vec2, into: Rect) -> Rect {
    let s = (into.width() / size.x).min(into.height() / size.y);
    Rect::from_center_size(into.center(), size * s)
}

fn stars_text(r: u8) -> String {
    let r = r.min(5) as usize;
    format!("{}{}", "★".repeat(r), "☆".repeat(5 - r))
}

/// Grayscale: the higher the rating, the brighter.
fn rating_color(r: u8) -> Color32 {
    match r {
        0 => Color32::from_gray(95),
        REJECT => Color32::from_gray(120),
        r if r >= PICKED_MIN => BRIGHT,
        _ => Color32::from_gray(180),
    }
}

// Grayscale palette.
const CANVAS: Color32 = Color32::from_gray(12);
const PANEL: Color32 = Color32::from_gray(19);
const SURFACE: Color32 = Color32::from_gray(30);
const LINE: Color32 = Color32::from_gray(38);
const FAINT: Color32 = Color32::from_gray(85);
const MUTED: Color32 = Color32::from_gray(125);
const TEXT: Color32 = Color32::from_gray(200);
const BRIGHT: Color32 = Color32::from_gray(245);

/// "Mar 2016".
fn month_name((y, m): (i32, u32)) -> String {
    format!("{} {y}", MONTHS[(m - 1) as usize])
}

/// Thin vertical rule between toolbar groups.
fn divider(ui: &mut egui::Ui) {
    ui.add_space(4.0);
    let (r, _) = ui.allocate_exact_size(vec2(1.0, 16.0), Sense::hover());
    ui.painter().rect_filled(r, 0.0, LINE);
    ui.add_space(4.0);
}

fn apply_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = PANEL;
        v.window_fill = Color32::from_gray(22);
        v.window_stroke = Stroke::new(1.0, LINE);
        v.window_corner_radius = 6.into();
        v.menu_corner_radius = 4.into();
        v.window_shadow = egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(120) };
        v.popup_shadow = egui::Shadow { offset: [0, 4], blur: 12, spread: 0, color: Color32::from_black_alpha(100) };
        v.extreme_bg_color = CANVAS;
        v.faint_bg_color = SURFACE;
        v.hyperlink_color = TEXT;
        v.warn_fg_color = TEXT;
        v.error_fg_color = BRIGHT;
        v.selection.bg_fill = Color32::from_gray(58);
        v.selection.stroke = Stroke::new(1.0, BRIGHT);

        let w = &mut v.widgets;
        w.noninteractive.bg_fill = PANEL;
        w.noninteractive.weak_bg_fill = PANEL;
        w.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        for (state, bg, fg) in [
            (&mut w.inactive, Color32::TRANSPARENT, Color32::from_gray(175)),
            (&mut w.hovered, SURFACE, BRIGHT),
            (&mut w.active, Color32::from_gray(44), BRIGHT),
            (&mut w.open, SURFACE, BRIGHT),
        ] {
            state.bg_fill = if bg == Color32::TRANSPARENT { SURFACE } else { bg };
            state.weak_bg_fill = bg;
            state.bg_stroke = Stroke::NONE;
            state.fg_stroke = Stroke::new(1.0, fg);
            state.corner_radius = 4.into();
            state.expansion = 0.0;
        }

        let s = &mut style.spacing;
        s.item_spacing = vec2(6.0, 6.0);
        s.button_padding = vec2(10.0, 5.0);
        s.interact_size.y = 26.0;
        s.window_margin = egui::Margin::same(18);
        s.menu_margin = egui::Margin::same(6);

        use egui::TextStyle as T;
        for (ts, size) in [(T::Body, 13.0), (T::Button, 13.0), (T::Small, 11.0), (T::Monospace, 12.0), (T::Heading, 16.0)] {
            if let Some(f) = style.text_styles.get_mut(&ts) {
                f.size = size;
            }
        }
    });
}
