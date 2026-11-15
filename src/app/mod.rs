//! The window: the queue on the left, preview and timeline in the middle, and the
//! selected video's settings on the right, in tabs. This file holds the state and what
//! happens when things change; `ui.rs` draws it and `timeline.rs` draws the timeline.

mod timeline;
mod ui;

use crate::config::{self, Persisted, Preset, SavedItem};
use crate::edit::{Edit, Range};
use crate::encode::{
    self, AudioMode, Backend, Codec, EncodeSettings, Engine, Family, FpsMode, JobSpec, ResolutionMode,
    SizePreset, Speed, TrackMode, Worker, WorkerMsg,
};
use crate::ffmpeg::{self, Av1Encoder, Caps, GpuEncoder, MediaInfo, Tools};
use crate::player::{MediaSource, Player};
use crate::services::{self, MediaMsg};
use crate::theme::{self, Palette};
use crate::util;
use eframe::egui::{self, Align2, Button, Color32, FontId, Id, Key, Modifiers, RichText, ScrollArea, Stroke, StrokeKind};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Containers offered in the file picker and accepted by drag-and-drop.
const SUPPORTED_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "mov", "avi", "webm", "m4v", "ts", "m2ts", "mts", "flv", "wmv", "3gp", "mpg", "mpeg", "ogv",
];
const MIN_CLIP: f64 = 0.1;

// ============================================================
// STATE
// ============================================================

#[derive(Clone, Debug, PartialEq)]
enum Status {
    Waiting,
    Encoding,
    Done { size_mib: f64 },
    Error,
    Cancelled,
}

impl Status {
    fn text(&self) -> String {
        match self {
            Status::Waiting => "Waiting".to_string(),
            Status::Encoding => "Encoding".to_string(),
            Status::Done { size_mib } => format!("Done • {size_mib:.2} MiB"),
            Status::Error => "Error".to_string(),
            Status::Cancelled => "Cancelled".to_string(),
        }
    }

    fn color(&self, p: &Palette) -> Color32 {
        match self {
            Status::Waiting | Status::Cancelled => p.weak,
            Status::Encoding => p.accent_ink,
            Status::Done { .. } => p.ok,
            Status::Error => p.bad,
        }
    }
}

struct Item {
    id: u64,
    path: PathBuf,
    info: MediaInfo,
    start: f64,
    end: f64,
    settings: EncodeSettings,
    status: Status,
    output_dir: String,
    output_name: String,
    /// True when the user has explicitly customized the filename.
    custom_name: bool,
    /// True while the item uses a stand-in codec because the remembered one (usually a GPU
    /// encoder) hasn't been detected yet. Cleared once detection finishes or the user picks one.
    codec_pending: bool,
    last_output: Option<PathBuf>,
    error: Option<String>,
    edit: Edit,
    /// SSIM of the last encode against the source.
    quality: Option<f64>,
    /// The last result was a stream copy.
    copied: bool,
    thumbs: Vec<egui::TextureHandle>,
    waveform: Option<Arc<Vec<f32>>>,
}

/// What undo and redo restore: everything about how one video gets encoded.
#[derive(Clone, PartialEq)]
struct Snapshot {
    start: f64,
    end: f64,
    edit: Edit,
    settings: EncodeSettings,
}

impl Item {
    fn snapshot(&self) -> Snapshot {
        Snapshot { start: self.start, end: self.end, edit: self.edit.clone(), settings: self.settings }
    }

    fn saved(&self) -> SavedItem {
        SavedItem {
            path: self.path.clone(),
            start: self.start,
            end: self.end,
            settings: self.settings,
            edit: self.edit.clone(),
            output_dir: self.output_dir.clone(),
            output_name: self.output_name.clone(),
            custom_name: self.custom_name,
            last_output: self.last_output.clone(),
        }
    }

    fn name(&self) -> String {
        util::file_name_lossy(&self.path)
    }

    fn output_path(&self) -> PathBuf {
        util::expand_tilde(&self.output_dir).join(util::sanitize_filename(&self.output_name))
    }

    fn media_source(&self) -> MediaSource {
        let (width, height) = MediaSource::preview_size(self.info.width, self.info.height);
        MediaSource {
            path: self.path.clone(),
            duration: self.info.duration,
            fps: self.info.fps,
            video_index: self.info.video_index,
            audio_tracks: self.info.audio_tracks,
            mix_audio: self.settings.tracks == TrackMode::Mix,
            track_volumes: (0..self.info.audio_tracks).map(|t| self.edit.volume(t)).collect(),
            width,
            height,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum DialogKind {
    Info,
    Warning,
    Error,
}

enum DialogAction {
    Close,
    EncodeAll,
    EncodeUnfinished,
    OpenFolder(PathBuf),
}

struct Dialog {
    kind: DialogKind,
    title: String,
    body: String,
    buttons: Vec<(String, DialogAction)>,
}

impl Dialog {
    fn simple(kind: DialogKind, title: impl Into<String>, body: impl Into<String>) -> Dialog {
        Dialog {
            kind,
            title: title.into(),
            body: body.into(),
            buttons: vec![("OK".to_string(), DialogAction::Close)],
        }
    }
}

enum ProbeMsg {
    Item(u64, PathBuf, Result<Box<MediaInfo>, String>),
    Finished(u64),
}

#[derive(Default)]
struct Batch {
    failures: Vec<String>,
    selected_first: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum DragTarget {
    Playhead,
    Start,
    End,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Edit,
    Encode,
    Audio,
    Output,
}

/// A part being marked on the timeline: the first press sets where it starts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MarkKind {
    Cut,
    Mute,
}

/// Which part of the crop box is being dragged on the preview.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CropHandle {
    Move,
    Corner(bool, bool),
}

/// Messages from background threads other than encoding.
enum BgMsg {
    Status(String),
    Error(String, String),
    Update(String),
}

struct SampleRun {
    item_id: u64,
    worker: Worker,
    /// Source time of the sample's first frame, and how fast the sample plays relative to
    /// the source: sample time t shows source time `start + t * speed`.
    src_start: f64,
    speed: f64,
    /// Filters that turn a source frame into what the encode was made from (crop, HDR).
    before_vf: Vec<String>,
}

/// Raw RGBA frames for the comparison: (original, encoded).
type FramePair = Option<(Vec<u8>, Vec<u8>)>;

/// The before/after view: the same frame from the source and from the encoded sample,
/// split by a line the user drags.
struct Compare {
    sample: PathBuf,
    source: PathBuf,
    duration: f64,
    fps: f64,
    /// Frame size both sides are shown at.
    width: usize,
    height: usize,
    src_start: f64,
    speed: f64,
    before_vf: String,
    /// Position in the sample, in seconds.
    t: f64,
    before: Option<egui::TextureHandle>,
    after: Option<egui::TextureHandle>,
    loading: Option<Receiver<FramePair>>,
    /// A position asked for while another was loading; loaded next.
    wanted: Option<f64>,
    /// "596 KiB • SSIM 0.988 (excellent)".
    summary: String,
    /// Magnification (1 = whole frame) and the frame point at the middle of the view (0..1).
    zoom: f32,
    center: egui::Vec2,
}

pub struct App {
    ctx: egui::Context,
    tools: Option<Tools>,
    missing: Vec<&'static str>,
    caps: Arc<Caps>,
    hardware_rx: Option<Receiver<(Vec<GpuEncoder>, Option<String>)>>,
    probe_tx: Sender<ProbeMsg>,
    probe_rx: Receiver<ProbeMsg>,
    probing: HashSet<PathBuf>,
    batches: HashMap<u64, Batch>,
    next_batch: u64,

    items: Vec<Item>,
    next_id: u64,
    selected: Option<u64>,
    player: Option<Player>,

    worker: Option<Worker>,
    processing: bool,
    cancel_requested: bool,
    run_done: usize,
    run_failed: usize,
    run_folders: Vec<PathBuf>,
    progress: f32,
    progress_detail: String,
    status_text: String,

    persisted: Persisted,
    /// Encoding settings new videos start with: whatever the user picked last.
    remembered: EncodeSettings,
    default_dir_buf: String,
    folder_buf: String,
    name_buf: String,
    dialogs: VecDeque<Dialog>,
    drag: Option<DragTarget>,
    resume_after_drag: bool,
    view_height: f32,

    tab: Tab,
    settings_open: bool,
    /// Visible part of the timeline (start, end) when zoomed in.
    view: Option<(f64, f64)>,
    mark: Option<(MarkKind, f64)>,
    crop_drag: Option<(CropHandle, egui::Pos2, crate::edit::Crop)>,
    start_buf: String,
    end_buf: String,
    undo: Vec<(u64, Snapshot)>,
    redo: Vec<(u64, Snapshot)>,
    undo_base: Option<(u64, Snapshot)>,
    media_tx: Sender<MediaMsg>,
    media_rx: Receiver<MediaMsg>,
    watcher: Option<services::Watcher>,
    watch_next: Instant,
    /// Saved edits for videos being re-added from the last session, by path.
    restore: HashMap<PathBuf, SavedItem>,
    sample: Option<SampleRun>,
    showing_sample: Option<PathBuf>,
    compare: Option<Compare>,
    /// Where the before/after line sits, 0 (all encoded) to 1 (all original). Kept between
    /// samples so trying settings one after another compares the same way.
    compare_split: f32,
    sample_status: String,
    bg_tx: Sender<BgMsg>,
    bg_rx: Receiver<BgMsg>,
    update_available: Option<String>,
    clipboard: Option<arboard::Clipboard>,
    gpu_key: String,
    preset_name: String,
    menu_installed: bool,
    /// Clips found in the watch folder that haven't been picked up for encoding yet.
    watch_added: HashSet<PathBuf>,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        tools: Result<Tools, Vec<&'static str>>,
        initial_files: Vec<PathBuf>,
    ) -> App {
        let persisted: Persisted = cc
            .storage
            .and_then(|s| eframe::get_value(s, config::KEY_SETTINGS))
            .unwrap_or_default();
        theme::apply(&cc.egui_ctx, persisted.theme);
        let saved_queue: Vec<SavedItem> = if persisted.restore_queue {
            cc.storage.and_then(|s| eframe::get_value(s, config::KEY_QUEUE)).unwrap_or_default()
        } else {
            Vec::new()
        };
        // Kept under its own key so a settings file from an older or newer version that
        // can't be read only resets the encoding choices, not everything else.
        let remembered: EncodeSettings = cc
            .storage
            .and_then(|s| eframe::get_value(s, config::KEY_ENCODE))
            .unwrap_or_default();
        let (probe_tx, probe_rx) = mpsc::channel();
        let (media_tx, media_rx) = mpsc::channel();
        let (bg_tx, bg_rx) = mpsc::channel();

        let mut app = App {
            ctx: cc.egui_ctx.clone(),
            tools: None,
            missing: Vec::new(),
            caps: Arc::new(Caps::default()),
            hardware_rx: None,
            probe_tx,
            probe_rx,
            probing: HashSet::new(),
            batches: HashMap::new(),
            next_batch: 0,
            items: Vec::new(),
            next_id: 0,
            selected: None,
            player: None,
            worker: None,
            processing: false,
            cancel_requested: false,
            run_done: 0,
            run_failed: 0,
            run_folders: Vec::new(),
            progress: 0.0,
            progress_detail: String::new(),
            status_text: "Ready.".to_string(),
            default_dir_buf: persisted.default_output_dir.clone(),
            persisted,
            remembered,
            folder_buf: String::new(),
            name_buf: String::new(),
            dialogs: VecDeque::new(),
            drag: None,
            resume_after_drag: false,
            view_height: 600.0,
            tab: Tab::Edit,
            settings_open: false,
            view: None,
            mark: None,
            crop_drag: None,
            start_buf: String::new(),
            end_buf: String::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            undo_base: None,
            media_tx,
            media_rx,
            watcher: None,
            watch_next: Instant::now(),
            restore: HashMap::new(),
            sample: None,
            showing_sample: None,
            compare: None,
            compare_split: 0.5,
            sample_status: String::new(),
            bg_tx,
            bg_rx,
            update_available: None,
            clipboard: None,
            gpu_key: String::new(),
            preset_name: String::new(),
            menu_installed: services::menu_installed(),
            watch_added: HashSet::new(),
        };

        match tools {
            Ok(tools) => {
                let mut caps = Caps::detect(&tools);
                // GPU results from the last launch, if nothing they depend on has changed, so a
                // remembered GPU codec is usable right away. They're checked again below.
                app.gpu_key = services::gpu_fingerprint(&tools);
                if let Some((gpu, hwaccel)) = services::load_gpu_cache(&app.gpu_key) {
                    caps.gpu = gpu;
                    caps.hwaccel = hwaccel;
                    caps.hardware_checked = true;
                }
                if !caps.has_any_encoder() {
                    app.status_text = "This FFmpeg build has no H.264, H.265 or AV1 encoder. \
                                       Checking for GPU encoders…"
                        .to_string();
                }
                // GPU encoder and hardware decoder checks take a few seconds; do them in the
                // background so the window opens immediately.
                let (tx, rx) = mpsc::channel();
                let (t, c, ctx) = (tools.clone(), caps.clone(), cc.egui_ctx.clone());
                thread::spawn(move || {
                    let _ = tx.send(ffmpeg::detect_hardware(&t, &c));
                    ctx.request_repaint();
                });
                app.hardware_rx = Some(rx);
                app.caps = Arc::new(caps);
                let mut player = Player::new(tools.clone(), app.persisted.volume, app.persisted.muted);
                player.set_hwaccel(app.caps.hwaccel.clone());
                app.player = Some(player);
                app.tools = Some(tools);
            }
            Err(missing) => app.missing = missing,
        }
        let mut paths = Vec::new();
        for saved in saved_queue {
            if saved.path.is_file() && !app.restore.contains_key(&saved.path) {
                paths.push(saved.path.clone());
                app.restore.insert(saved.path.clone(), saved);
            }
        }
        paths.extend(initial_files);
        if !paths.is_empty() {
            app.add_paths(paths);
        }
        app.restart_watcher();
        if app.persisted.check_updates {
            let tx = app.bg_tx.clone();
            let ctx = app.ctx.clone();
            thread::spawn(move || {
                if let Ok(Some(tag)) = services::newer_release(env!("CARGO_PKG_REPOSITORY"), env!("CARGO_PKG_VERSION")) {
                    let _ = tx.send(BgMsg::Update(tag));
                    ctx.request_repaint();
                }
            });
        }
        app
    }

    fn palette(&self) -> Palette {
        theme::current(&self.ctx)
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.items.iter().position(|i| i.id == id)
    }

    fn selected_index(&self) -> Option<usize> {
        self.selected.and_then(|id| self.index_of(id))
    }

    fn warn(&mut self, title: &str, body: impl Into<String>) {
        self.dialogs.push_back(Dialog::simple(DialogKind::Warning, title, body));
    }

    // ========================================================
    // BACKGROUND RESULTS
    // ========================================================

    fn poll(&mut self) {
        if let Some(rx) = &self.hardware_rx {
            if let Ok((gpu, hwaccel)) = rx.try_recv() {
                let mut caps = (*self.caps).clone();
                caps.gpu = gpu;
                caps.hwaccel = hwaccel.clone();
                caps.hardware_checked = true;
                let had_encoder = self.caps.has_any_encoder();
                self.caps = Arc::new(caps);
                self.hardware_rx = None;
                if let Some(p) = &mut self.player {
                    p.set_hwaccel(hwaccel);
                }
                if let Some(tools) = &self.tools {
                    services::save_gpu_cache(&self.gpu_key, &self.caps.gpu, &self.caps.hwaccel);
                    let _ = tools;
                }
                // Videos added before detection finished may be waiting on the remembered GPU
                // codec, and a codec from the cached results may have disappeared; give each
                // waiting video its real codec now.
                let options = self.caps.codec_options();
                for index in 0..self.items.len() {
                    let item = &self.items[index];
                    let vanished = !options.contains(&item.settings.codec);
                    if !(item.codec_pending || vanished) {
                        continue;
                    }
                    let wanted = if vanished && !item.codec_pending { item.settings.codec } else { self.remembered.codec };
                    let (codec, _) = self.caps.resolve_codec(wanted);
                    self.items[index].codec_pending = false;
                    if self.items[index].status == Status::Waiting && self.items[index].settings.codec != codec {
                        let settings = EncodeSettings { codec, ..self.items[index].settings };
                        self.apply_settings(index, settings);
                    }
                }
                if !had_encoder {
                    if self.caps.has_any_encoder() {
                        self.status_text = "Ready.".to_string();
                    } else {
                        self.status_text = "This FFmpeg build can't encode H.264, H.265 or AV1, \
                                            so nothing can be compressed. Install a full FFmpeg build."
                            .to_string();
                    }
                }
            }
        }

        while let Ok(msg) = self.probe_rx.try_recv() {
            match msg {
                ProbeMsg::Item(batch, path, result) => {
                    self.probing.remove(&path);
                    match result {
                        Ok(info) => {
                            let id = self.add_item(path, *info);
                            let first = self.batches.get_mut(&batch).map(|b| {
                                let first = !b.selected_first;
                                b.selected_first = true;
                                first
                            });
                            if first == Some(true) && self.drag.is_none() {
                                self.select(Some(id));
                            }
                        }
                        Err(e) => {
                            if let Some(b) = self.batches.get_mut(&batch) {
                                b.failures.push(format!("{}\n{e}", util::file_name_lossy(&path)));
                            }
                        }
                    }
                }
                ProbeMsg::Finished(batch) => {
                    if let Some(b) = self.batches.remove(&batch) {
                        if !b.failures.is_empty() {
                            self.warn("Could not read video", b.failures.join("\n\n"));
                        }
                    }
                    if self.probing.is_empty() && !self.processing {
                        self.status_text = "Ready.".to_string();
                    }
                }
            }
        }

        let messages: Vec<WorkerMsg> = match &self.worker {
            Some(w) => w.rx.try_iter().collect(),
            None => Vec::new(),
        };
        for msg in messages {
            self.handle_worker(msg);
        }
        self.poll_background();
    }

    /// Thumbnails, waveforms, sample encodes, network results and the watch folder.
    fn poll_background(&mut self) {
        while let Ok(msg) = self.media_rx.try_recv() {
            match msg {
                MediaMsg::Thumbs { path, thumbs } => {
                    if let Some(item) = self.items.iter_mut().find(|i| i.path == path) {
                        item.thumbs = thumbs
                            .into_iter()
                            .enumerate()
                            .map(|(n, t)| {
                                let image = egui::ColorImage::from_rgba_unmultiplied([t.width, t.height], &t.rgba);
                                self.ctx.load_texture(format!("thumb-{}-{n}", item.id), image, egui::TextureOptions::LINEAR)
                            })
                            .collect();
                    }
                }
                MediaMsg::Waveform { path, peaks } => {
                    if let Some(item) = self.items.iter_mut().find(|i| i.path == path) {
                        item.waveform = Some(Arc::new(peaks));
                    }
                }
            }
        }
        while let Ok(msg) = self.bg_rx.try_recv() {
            match msg {
                BgMsg::Status(text) => self.status_text = text,
                BgMsg::Error(title, body) => self.dialogs.push_back(Dialog::simple(DialogKind::Error, title, body)),
                BgMsg::Update(tag) => self.update_available = Some(tag),
            }
        }
        let sample_msgs: Vec<WorkerMsg> = match &self.sample {
            Some(run) => run.worker.rx.try_iter().collect(),
            None => Vec::new(),
        };
        for msg in sample_msgs {
            self.handle_sample(msg);
        }
        self.poll_compare();
        if self.watcher.is_some() && Instant::now() >= self.watch_next {
            self.watch_next = Instant::now() + Duration::from_secs(2);
            let found = self.watcher.as_mut().map(|w| w.poll(SUPPORTED_EXTENSIONS)).unwrap_or_default();
            if !found.is_empty() {
                self.status_text = format!(
                    "New in the watched folder: {}",
                    found.iter().map(|p| util::file_name_lossy(p)).collect::<Vec<_>>().join(", ")
                );
                self.watch_added.extend(found.iter().cloned());
                let nothing_selected = self.selected.is_none();
                self.add_files_with(found, nothing_selected);
            }
        }
        // Start encoding watched clips once they've been read, if the user asked for that.
        if self.persisted.watch_auto_encode && !self.processing && self.probing.is_empty() && !self.watch_added.is_empty() {
            let ready: Vec<PathBuf> = self.watch_added.drain().collect();
            if ready.iter().any(|p| self.items.iter().any(|i| &i.path == p && i.status == Status::Waiting)) {
                self.begin_run(false);
            }
        }
    }

    pub(crate) fn restart_watcher(&mut self) {
        self.watcher = None;
        let dir = util::expand_tilde(&self.persisted.watch_dir);
        if self.persisted.watch_enabled && dir.is_dir() {
            self.watcher = Some(services::Watcher::new(dir, SUPPORTED_EXTENSIONS));
        }
    }

    fn handle_worker(&mut self, msg: WorkerMsg) {
        let Some(item_id) = self.worker.as_ref().map(|w| w.item_id) else { return };
        let index = self.index_of(item_id);
        match msg {
            WorkerMsg::Progress { fraction, speed, eta_secs } => {
                self.progress = fraction;
                let mut parts = vec![format!("{:.0}%", fraction * 100.0)];
                if let Some(s) = speed {
                    parts.push(format!("{s:.2}× real time"));
                }
                if let Some(eta) = eta_secs.filter(|e| *e > 1.0 && fraction < 0.999) {
                    parts.push(format!("about {} left", util::format_time(eta)));
                }
                self.progress_detail = parts.join("   •   ");
            }
            WorkerMsg::Status(text) => self.status_text = text,
            WorkerMsg::Done { path, size, quality, copied } => {
                let size_mib = util::mib(size);
                if let Some(i) = index {
                    self.items[i].status = Status::Done { size_mib };
                    self.items[i].last_output = Some(path.clone());
                    self.items[i].error = None;
                    self.items[i].quality = quality;
                    self.items[i].copied = copied;
                }
                self.after_output(&path);
                if let Some(dir) = path.parent() {
                    if !self.run_folders.iter().any(|d| d == dir) {
                        self.run_folders.push(dir.to_path_buf());
                    }
                }
                self.run_done += 1;
                let how = match (copied, quality) {
                    (true, _) => " • copied without re-encoding".to_string(),
                    (false, Some(q)) => format!(" • quality {}", encode::quality_label(q).to_lowercase()),
                    _ => String::new(),
                };
                self.status_text = format!("Created: {} ({size_mib:.2} MiB{how})", util::file_name_lossy(&path));
                self.worker = None;
                if self.cancel_requested {
                    self.stop_run("Stopped. Videos that hadn't started are still waiting.");
                } else {
                    self.process_next();
                }
            }
            WorkerMsg::Failed(error) => {
                if let Some(i) = index {
                    self.items[i].status = Status::Error;
                    self.items[i].error = Some(error.clone());
                }
                self.run_failed += 1;
                self.status_text = "Encoding failed.".to_string();
                let name = index.map(|i| self.items[i].name()).unwrap_or_default();
                self.dialogs.push_back(Dialog::simple(
                    DialogKind::Error,
                    "FFmpeg encoding error",
                    if name.is_empty() { error } else { format!("{name}\n\n{error}") },
                ));
                self.worker = None;
                // Continue with remaining videos.
                if self.cancel_requested {
                    self.stop_run("Stopped. Videos that hadn't started are still waiting.");
                } else {
                    self.process_next();
                }
            }
            WorkerMsg::Cancelled => {
                if let Some(i) = index {
                    self.items[i].status = Status::Cancelled;
                }
                self.worker = None;
                self.stop_run("Cancelled. Videos that hadn't started are still waiting.");
            }
        }
    }

    // ========================================================
    // ADD FILES
    // ========================================================

    fn add_files(&mut self, paths: Vec<PathBuf>) {
        self.add_files_with(paths, true);
    }

    /// `select`: show the first new video once it's read. Videos from the watch folder
    /// don't, so they never pull the user away from what they're editing.
    fn add_files_with(&mut self, paths: Vec<PathBuf>, select: bool) {
        let Some(tools) = self.tools.clone() else { return };
        let mut batch_paths = Vec::new();
        for path in paths {
            if !path.is_file() {
                continue;
            }
            let ext_ok = path
                .extension()
                .map(|e| SUPPORTED_EXTENSIONS.contains(&e.to_string_lossy().to_lowercase().as_str()))
                .unwrap_or(false);
            if !ext_ok {
                continue;
            }
            let path = path.canonicalize().unwrap_or(path);
            // Don't add duplicates.
            if self.items.iter().any(|i| i.path == path) || self.probing.contains(&path) {
                continue;
            }
            if batch_paths.contains(&path) {
                continue;
            }
            self.probing.insert(path.clone());
            batch_paths.push(path);
        }
        if batch_paths.is_empty() {
            return;
        }

        let batch = self.next_batch;
        self.next_batch += 1;
        self.batches.insert(batch, Batch { failures: Vec::new(), selected_first: !select });
        if !self.processing {
            self.status_text = "Reading video info…".to_string();
        }
        let tx = self.probe_tx.clone();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            // Read several files at once (FFprobe mostly waits on the disk and process
            // startup), but hand them over in the original order so the queue matches it.
            let workers = thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, 4);
            let next = AtomicUsize::new(0);
            let (result_tx, result_rx) = mpsc::channel();
            thread::scope(|s| {
                for _ in 0..workers.min(batch_paths.len()) {
                    let (result_tx, paths, next, tools) = (result_tx.clone(), &batch_paths, &next, &tools);
                    s.spawn(move || {
                        loop {
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            let Some(path) = paths.get(index) else { break };
                            let _ = result_tx.send((index, ffmpeg::probe(tools, path)));
                        }
                    });
                }
                drop(result_tx);
                let mut finished = BTreeMap::new();
                let mut emit = 0;
                for (index, result) in result_rx {
                    finished.insert(index, result);
                    while let Some(result) = finished.remove(&emit) {
                        let _ = tx.send(ProbeMsg::Item(batch, batch_paths[emit].clone(), result.map(Box::new)));
                        ctx.request_repaint();
                        emit += 1;
                    }
                }
            });
            let _ = tx.send(ProbeMsg::Finished(batch));
            ctx.request_repaint();
        });
    }

    fn add_item(&mut self, path: PathBuf, info: MediaInfo) -> u64 {
        let saved = self.restore.remove(&path);
        let wanted = saved.as_ref().map(|s| s.settings).unwrap_or(self.remembered);
        let (codec, codec_pending) = self.caps.resolve_codec(wanted.codec);
        let settings = EncodeSettings { codec, ..wanted };
        if let Some(tools) = &self.tools {
            let ctx = self.ctx.clone();
            services::spawn_media(tools.clone(), path.clone(), &info, self.media_tx.clone(), move || ctx.request_repaint());
        }
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(Item {
            id,
            output_name: encode::output_name_for(&path, &settings),
            path,
            start: 0.0,
            end: info.duration,
            info,
            settings,
            status: Status::Waiting,
            output_dir: self.persisted.default_output_dir.clone(),
            custom_name: false,
            codec_pending,
            last_output: None,
            error: None,
            edit: Edit::default(),
            quality: None,
            copied: false,
            thumbs: Vec::new(),
            waveform: None,
        });
        if let Some(saved) = saved {
            let item = self.items.last_mut().expect("just added");
            let duration = item.info.duration;
            item.start = saved.start.clamp(0.0, duration);
            item.end = if saved.end > item.start { saved.end.min(duration) } else { duration };
            item.edit = saved.edit;
            if !saved.output_dir.is_empty() {
                item.output_dir = saved.output_dir;
            }
            if saved.custom_name {
                item.output_name = saved.output_name;
                item.custom_name = true;
            }
            if let Some(out) = saved.last_output.filter(|p| p.is_file()) {
                let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
                item.status = Status::Done { size_mib: util::mib(size) };
                item.last_output = Some(out);
            }
        }
        id
    }

    fn choose_files(&mut self) {
        let start_dir = self
            .persisted
            .last_add_dir
            .clone()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        let files = rfd::FileDialog::new()
            .set_title("Select videos")
            .set_directory(start_dir)
            .add_filter("Videos", SUPPORTED_EXTENSIONS)
            .pick_files();
        if let Some(files) = files {
            if let Some(dir) = files.first().and_then(|f| f.parent()) {
                self.persisted.last_add_dir = Some(dir.to_string_lossy().into_owned());
            }
            self.add_files(files);
        }
    }

    // ========================================================
    // SELECTION
    // ========================================================

    fn select(&mut self, id: Option<u64>) {
        if self.selected == id {
            return;
        }
        self.force_select(id);
    }

    fn force_select(&mut self, id: Option<u64>) {
        self.selected = id;
        self.view = None;
        self.mark = None;
        self.crop_drag = None;
        self.showing_sample = None;
        self.compare = None;
        self.undo_base = self.selected_index().map(|i| (self.items[i].id, self.items[i].snapshot()));
        self.sync_buffers();
        let source = self.selected_index().map(|i| (self.items[i].media_source(), self.items[i].start));
        if let Some(player) = &mut self.player {
            match source {
                Some((src, at)) => player.load(src, at),
                None => player.unload(),
            }
        }
    }

    /// Reloads the folder and filename text boxes from the selected item.
    fn sync_buffers(&mut self) {
        match self.selected_index() {
            Some(i) => {
                self.folder_buf = self.items[i].output_dir.clone();
                self.name_buf = self.items[i].output_name.clone();
                self.start_buf = util::format_time_precise(self.items[i].start);
                self.end_buf = util::format_time_precise(self.items[i].end);
            }
            None => {
                self.folder_buf.clear();
                self.name_buf.clear();
                self.start_buf.clear();
                self.end_buf.clear();
            }
        }
    }

    fn remove_selected(&mut self) {
        if self.processing {
            return;
        }
        let Some(row) = self.selected_index() else { return };
        self.items.remove(row);
        if self.items.is_empty() {
            self.force_select(None);
        } else {
            let next = self.items[row.min(self.items.len() - 1)].id;
            self.force_select(Some(next));
        }
    }

    fn clear_queue(&mut self) {
        if self.processing {
            return;
        }
        self.items.clear();
        self.force_select(None);
    }

    // ========================================================
    // TRIM
    // ========================================================

    fn set_start_here(&mut self) {
        let (Some(i), Some(player)) = (self.selected_index(), &self.player) else { return };
        let position = player.position();
        if position >= self.items[i].end {
            self.warn("Invalid start", "Start must be before the end point.");
            return;
        }
        self.items[i].start = position;
    }

    fn set_end_here(&mut self) {
        let (Some(i), Some(player)) = (self.selected_index(), &self.player) else { return };
        let position = player.position();
        if position <= self.items[i].start {
            self.warn("Invalid end", "End must be after the start point.");
            return;
        }
        self.items[i].end = position;
    }

    // ========================================================
    // OUTPUT
    // ========================================================

    fn commit_default_dir(&mut self) {
        let folder = self.default_dir_buf.trim().to_string();
        if folder.is_empty() {
            self.default_dir_buf = self.persisted.default_output_dir.clone();
        } else {
            self.persisted.default_output_dir = folder.clone();
            self.default_dir_buf = folder;
        }
    }

    fn choose_default_folder(&mut self) {
        let start = util::expand_tilde(&self.persisted.default_output_dir);
        let mut dialog = rfd::FileDialog::new().set_title("Choose default output folder");
        if start.is_dir() {
            dialog = dialog.set_directory(start);
        }
        if let Some(folder) = dialog.pick_folder() {
            let folder = folder.to_string_lossy().into_owned();
            self.persisted.default_output_dir = folder.clone();
            self.default_dir_buf = folder;
        }
    }

    fn use_default_for_selected(&mut self) {
        let Some(i) = self.selected_index() else { return };
        self.items[i].output_dir = self.persisted.default_output_dir.clone();
        self.sync_buffers();
    }

    fn choose_output_folder(&mut self) {
        let Some(i) = self.selected_index() else { return };
        let start = util::expand_tilde(&self.items[i].output_dir);
        let mut dialog = rfd::FileDialog::new().set_title("Choose output folder");
        if start.is_dir() {
            dialog = dialog.set_directory(start);
        }
        if let Some(folder) = dialog.pick_folder() {
            self.items[i].output_dir = folder.to_string_lossy().into_owned();
            self.sync_buffers();
        }
    }

    fn commit_output_folder(&mut self) {
        let Some(i) = self.selected_index() else { return };
        let folder = self.folder_buf.trim();
        if folder.is_empty() {
            self.folder_buf = self.items[i].output_dir.clone();
        } else {
            self.items[i].output_dir = folder.to_string();
        }
    }

    fn commit_output_name(&mut self) {
        let Some(i) = self.selected_index() else { return };
        let name = util::sanitize_filename(&self.name_buf);
        if name != self.items[i].output_name {
            self.items[i].output_name = name.clone();
            self.items[i].custom_name = true;
        }
        self.name_buf = name;
    }

    fn reset_output_name(&mut self) {
        let Some(i) = self.selected_index() else { return };
        let item = &mut self.items[i];
        item.output_name = encode::output_name_for(&item.path, &item.settings);
        item.custom_name = false;
        self.sync_buffers();
    }

    /// Applies new settings, keeping auto-generated filenames in step with the codec.
    fn apply_settings(&mut self, index: usize, settings: EncodeSettings) {
        let item = &mut self.items[index];
        let codec_changed =
            item.settings.codec.family != settings.codec.family || item.settings.format != settings.format;
        let tracks_changed = item.settings.tracks != settings.tracks;
        if item.settings.codec != settings.codec {
            item.codec_pending = false;
        }
        item.settings = settings;
        if codec_changed && !item.custom_name {
            item.output_name = encode::output_name_for(&item.path, &settings);
        } else if item.settings.format != settings.format || codec_changed {
            item.output_name = encode::with_format_extension(&item.output_name, settings.format);
        }
        if Some(item.id) == self.selected {
            if codec_changed {
                self.sync_buffers();
            }
            if tracks_changed {
                // The preview mixes tracks the same way the encode will.
                let (src, at) = (self.items[index].media_source(), self.items[index].start);
                if let Some(p) = &mut self.player {
                    let pos = p.position();
                    p.load(src, at);
                    p.seek(pos);
                }
            }
        }
    }

    fn apply_to_all(&mut self) {
        let Some(i) = self.selected_index() else { return };
        let settings = self.items[i].settings;
        for index in 0..self.items.len() {
            if index != i && self.items[index].status != Status::Encoding {
                self.apply_settings(index, settings);
            }
        }
        self.status_text = format!("Applied these encoding settings to all {} videos.", self.items.len());
    }

    // ========================================================
    // QUEUE PROCESSING
    // ========================================================

    fn start_queue(&mut self) {
        if self.processing {
            return;
        }
        if self.items.is_empty() {
            self.warn("Queue empty", "Add at least one video.");
            return;
        }
        if !self.caps.has_any_encoder() {
            self.dialogs.push_back(Dialog::simple(
                DialogKind::Error,
                "No encoder available",
                "This FFmpeg build can't encode H.264, H.265 or AV1. Install a full FFmpeg build \
                 (for example from your package manager or ffmpeg.org) and restart the app.",
            ));
            return;
        }

        // Make sure every output directory exists.
        for index in 0..self.items.len() {
            let output_dir = util::expand_tilde(&self.items[index].output_dir);
            if let Err(e) = std::fs::create_dir_all(&output_dir) {
                let id = self.items[index].id;
                self.select(Some(id));
                self.dialogs.push_back(Dialog::simple(
                    DialogKind::Error,
                    "Output folder error",
                    format!("Could not create:\n\n{}\n\n{e}", output_dir.display()),
                ));
                return;
            }
            let item = &mut self.items[index];
            item.output_dir = output_dir.to_string_lossy().into_owned();
            item.output_name = util::sanitize_filename(&item.output_name);
            if item.end <= item.start || item.edit.output_duration(item.start, item.end) < crate::edit::MIN_PART {
                let (id, name) = (item.id, item.name());
                self.select(Some(id));
                self.warn("Invalid trim", format!("{name}\n\nThe end time must be after the start time."));
                return;
            }
        }
        self.sync_buffers();

        let finished = self
            .items
            .iter()
            .filter(|i| matches!(i.status, Status::Done { .. }))
            .count();
        if finished > 0 && finished < self.items.len() {
            let plural = if finished == 1 { "video is" } else { "videos are" };
            self.dialogs.push_back(Dialog {
                kind: DialogKind::Info,
                title: "Some videos are already done".to_string(),
                body: format!(
                    "{finished} {plural} already compressed. Encode only the rest, or everything again? \
                     Encoding again makes new copies and never overwrites the earlier files."
                ),
                buttons: vec![
                    ("Only the rest".to_string(), DialogAction::EncodeUnfinished),
                    ("Everything again".to_string(), DialogAction::EncodeAll),
                    ("Cancel".to_string(), DialogAction::Close),
                ],
            });
            return;
        }
        self.begin_run(true);
    }

    fn begin_run(&mut self, everything: bool) {
        for item in &mut self.items {
            if everything || !matches!(item.status, Status::Done { .. }) {
                item.status = Status::Waiting;
            }
        }
        self.processing = true;
        self.cancel_requested = false;
        self.run_done = 0;
        self.run_failed = 0;
        self.run_folders.clear();
        self.progress = 0.0;
        self.progress_detail.clear();
        self.process_next();
    }

    fn process_next(&mut self) {
        let Some(index) = self.items.iter().position(|i| i.status == Status::Waiting) else {
            self.encoding_finished();
            return;
        };
        let Some(tools) = self.tools.clone() else { return };
        let total = self.items.len();
        let item = &mut self.items[index];
        item.status = Status::Encoding;
        item.error = None;
        self.progress = 0.0;
        self.progress_detail.clear();
        self.status_text = format!("Encoding {}/{}: {}", index + 1, total, item.name());

        let spec = self.job_spec(index);
        self.worker = Some({ let ctx = self.ctx.clone(); encode::spawn(tools, self.caps.clone(), spec, move || ctx.request_repaint()) });
    }

    /// Everything the encoder needs for one video, with the current settings.
    fn job_spec(&self, index: usize) -> JobSpec {
        let item = &self.items[index];
        JobSpec {
            item_id: item.id,
            input: item.path.clone(),
            output_dir: util::expand_tilde(&item.output_dir),
            output_name: util::sanitize_filename(&item.output_name),
            start: item.start,
            end: item.end,
            settings: item.settings,
            info: item.info.clone(),
            target_bytes: self.persisted.target_bytes(),
            edit: item.edit.clone(),
            copy_if_fits: self.persisted.copy_if_fits,
            measure_quality: self.persisted.measure_quality,
        }
    }

    fn encoding_finished(&mut self) {
        self.processing = false;
        self.worker = None;
        self.progress = 1.0;
        self.progress_detail.clear();
        self.status_text = "All videos finished.".to_string();
        self.ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
        if self.persisted.notify_when_done {
            let body = if self.run_failed > 0 {
                format!("{} finished, {} failed.", self.run_done, self.run_failed)
            } else {
                format!("{} video{} ready.", self.run_done, if self.run_done == 1 { "" } else { "s" })
            };
            services::notify("ClipCram finished", &body);
        }

        let mut body =
            "All videos have been processed.\n\nThe original files were not modified or deleted.".to_string();
        if self.run_failed > 0 {
            body.push_str(&format!("\n\n{} finished, {} failed.", self.run_done, self.run_failed));
        }
        let mut buttons = Vec::new();
        if self.run_folders.len() == 1 {
            buttons.push(("Open output folder".to_string(), DialogAction::OpenFolder(self.run_folders[0].clone())));
        }
        buttons.push(("OK".to_string(), DialogAction::Close));
        self.dialogs.push_back(Dialog {
            kind: DialogKind::Info,
            title: "Finished".to_string(),
            body,
            buttons,
        });
    }

    fn cancel_encoding(&mut self) {
        if let Some(w) = &self.worker {
            w.cancel.cancel();
            self.cancel_requested = true;
            self.status_text = "Cancelling…".to_string();
        }
    }

    fn stop_run(&mut self, message: &str) {
        self.processing = false;
        self.cancel_requested = false;
        self.progress = 0.0;
        self.progress_detail.clear();
        self.status_text = message.to_string();
    }

    // ========================================================
    // INPUT
    // ========================================================

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if !dropped.is_empty() {
            self.add_paths(dropped);
        }
    }

    /// Like `add_files`, but a folder adds the videos inside it.
    fn add_paths(&mut self, dropped: Vec<PathBuf>) {
        let mut paths = Vec::new();
        for p in dropped {
            if p.is_dir() {
                if let Ok(rd) = std::fs::read_dir(&p) {
                    let mut inner: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
                    inner.sort();
                    paths.extend(inner);
                }
            } else {
                paths.push(p);
            }
        }
        self.add_files(paths);
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() || !self.dialogs.is_empty() {
            return;
        }
        // Redo first: egui's key matching ignores an extra Shift, so Ctrl+Z would also
        // catch Ctrl+Shift+Z.
        let (undo, redo) = ctx.input_mut(|inp| {
            let redo = inp.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) || inp.consume_key(Modifiers::COMMAND, Key::Y);
            let undo = !redo && inp.consume_key(Modifiers::COMMAND, Key::Z);
            (undo, redo)
        });
        if redo {
            self.redo_edit();
        } else if undo {
            self.undo_edit();
        }
        let Some(i) = self.selected_index() else { return };
        let (start, end) = (self.items[i].start, self.items[i].end);
        let pressed = |k: Key| ctx.input_mut(|inp| inp.consume_key(Modifiers::NONE, k));
        let shift = |k: Key| ctx.input_mut(|inp| inp.consume_key(Modifiers::SHIFT, k));
        let (sl, sr) = (shift(Key::ArrowLeft), shift(Key::ArrowRight));
        let keys = [
            Key::Space, Key::I, Key::O, Key::ArrowLeft, Key::ArrowRight, Key::Comma, Key::Period, Key::Home, Key::End,
            Key::C, Key::M, Key::J, Key::K, Key::L, Key::Escape,
        ];
        let down: Vec<Key> = keys.into_iter().filter(|k| pressed(*k)).collect();
        let has = |k: Key| down.contains(&k);
        if has(Key::I) {
            self.set_start_here();
        }
        if has(Key::O) {
            self.set_end_here();
        }
        if has(Key::C) {
            self.toggle_mark(MarkKind::Cut);
        }
        if has(Key::M) {
            self.toggle_mark(MarkKind::Mute);
        }
        if has(Key::Escape) {
            self.mark = None;
        }
        let Some(player) = &mut self.player else { return };
        let pos = player.position();
        if has(Key::Space) || has(Key::K) {
            player.toggle();
        }
        if sl {
            player.seek(pos - 1.0);
        }
        if sr {
            player.seek(pos + 1.0);
        }
        if has(Key::ArrowLeft) || has(Key::J) {
            player.seek(pos - 5.0);
        }
        if has(Key::ArrowRight) || has(Key::L) {
            player.seek(pos + 5.0);
        }
        if has(Key::Comma) {
            player.step_frame(-1.0);
        }
        if has(Key::Period) {
            player.step_frame(1.0);
        }
        if has(Key::Home) {
            player.seek(start);
        }
        if has(Key::End) {
            player.seek(end);
        }
    }

    // ========================================================
    // CUT AND MUTE MARKS, UNDO
    // ========================================================

    /// First press marks where a part starts, the second cuts or mutes up to the playhead.
    fn toggle_mark(&mut self, kind: MarkKind) {
        let (Some(i), Some(player)) = (self.selected_index(), &self.player) else { return };
        let at = player.position();
        match self.mark {
            Some((k, from)) if k == kind => {
                self.mark = None;
                let range = Range::new(from, at);
                if range.len() < crate::edit::MIN_PART {
                    self.status_text = "That part is too short; mark a start and an end farther apart.".to_string();
                    return;
                }
                let item = &mut self.items[i];
                match kind {
                    MarkKind::Cut => item.edit.add_cut(range),
                    MarkKind::Mute => item.edit.add_mute(range),
                }
                self.status_text = format!(
                    "{} {} – {}.",
                    if kind == MarkKind::Cut { "Cut out" } else { "Muted" },
                    util::format_time_precise(range.start),
                    util::format_time_precise(range.end)
                );
            }
            _ => {
                self.mark = Some((kind, at));
                self.status_text = format!(
                    "Move to where the part ends and press {} again (Esc to cancel).",
                    if kind == MarkKind::Cut { "C" } else { "M" }
                );
            }
        }
    }

    /// Records an undo step whenever the selected video's settings have settled on a new
    /// state. Waiting until no button is held and no field is being typed in makes a drag
    /// or a typed time one step rather than dozens.
    fn track_undo(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.pointer.any_down()) || ctx.wants_keyboard_input() {
            return;
        }
        let Some(i) = self.selected_index() else { return };
        let current = (self.items[i].id, self.items[i].snapshot());
        match &self.undo_base {
            Some(base) if base.0 == current.0 && base.1 != current.1 => {
                let base = self.undo_base.replace(current).expect("checked");
                self.undo.push(base);
                if self.undo.len() > 200 {
                    self.undo.remove(0);
                }
                self.redo.clear();
            }
            Some(base) if base.0 == current.0 => {}
            _ => self.undo_base = Some(current),
        }
    }

    fn restore_snapshot(&mut self, id: u64, snap: Snapshot) -> Option<Snapshot> {
        let index = self.index_of(id)?;
        if self.items[index].status == Status::Encoding {
            return None;
        }
        let before = self.items[index].snapshot();
        let item = &mut self.items[index];
        item.start = snap.start;
        item.end = snap.end;
        item.edit = snap.edit.clone();
        self.apply_settings(index, snap.settings);
        if self.selected != Some(id) {
            self.select(Some(id));
        }
        self.undo_base = Some((id, snap));
        self.sync_buffers();
        self.reload_preview();
        Some(before)
    }

    fn undo_edit(&mut self) {
        self.track_undo_now();
        if let Some((id, snap)) = self.undo.pop() {
            if let Some(before) = self.restore_snapshot(id, snap) {
                self.redo.push((id, before));
                self.status_text = "Undone.".to_string();
            }
        }
    }

    fn redo_edit(&mut self) {
        if let Some((id, snap)) = self.redo.pop() {
            if let Some(before) = self.restore_snapshot(id, snap) {
                self.undo.push((id, before));
                self.status_text = "Redone.".to_string();
            }
        }
    }

    /// Records any pending change before undoing, so the latest edit is the one undone.
    fn track_undo_now(&mut self) {
        if let (Some(i), Some(base)) = (self.selected_index(), &self.undo_base) {
            let current = self.items[i].snapshot();
            if base.0 == self.items[i].id && base.1 != current {
                let base = self.undo_base.replace((self.items[i].id, current)).expect("checked");
                self.undo.push(base);
                self.redo.clear();
            }
        }
    }

    /// Reloads the preview after settings that change what it plays (track mixing, volumes).
    fn reload_preview(&mut self) {
        let Some(i) = self.selected_index() else { return };
        if self.showing_sample.is_some() {
            return;
        }
        let src = self.items[i].media_source();
        if let Some(p) = &mut self.player {
            let pos = p.position();
            p.load(src, pos);
        }
    }

    // ========================================================
    // SAMPLE PREVIEW
    // ========================================================

    /// Encodes three seconds from the middle of the clip with the real settings and bitrate,
    /// then plays it, to judge quality before the full encode.
    fn start_sample(&mut self) {
        let (Some(i), Some(tools)) = (self.selected_index(), self.tools.clone()) else { return };
        if self.sample.is_some() || self.processing {
            return;
        }
        let item = &self.items[i];
        let kept = item.edit.kept(item.start, item.end);
        let Some(longest) = kept.iter().copied().max_by(|a, b| a.len().total_cmp(&b.len())) else { return };
        let full = item.edit.output_duration(item.start, item.end).max(0.1);
        let window = (3.0 * item.edit.speed()).min(longest.len());
        let mid = (longest.start + longest.end) / 2.0;
        let (start, end) = ((mid - window / 2.0).max(longest.start), (mid + window / 2.0).min(longest.end));
        let mut edit = item.edit.clone();
        edit.cuts.clear();
        let sample_secs = (end - start) / edit.speed();
        let target_bytes = (self.persisted.target_bytes() as f64 * sample_secs / full) as u64;
        let dir = std::env::temp_dir().join(format!("clipcram-sample-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let spec = JobSpec {
            item_id: item.id,
            input: item.path.clone(),
            output_dir: dir,
            output_name: encode::with_format_extension(&format!("sample-{}", item.id), item.settings.format),
            start,
            end,
            settings: item.settings,
            info: item.info.clone(),
            target_bytes: target_bytes.max(16_384),
            edit,
            copy_if_fits: false,
            measure_quality: true,
        };
        let mut before_vf = Vec::new();
        if let Some(c) = item.edit.effective_crop(item.info.width, item.info.height) {
            before_vf.push(format!("crop={}:{}:{}:{}", c.w, c.h, c.x, c.y));
        }
        if let (Some(hdr), true) = (&item.info.hdr, item.settings.hdr_to_sdr && self.caps.tonemap) {
            before_vf.push(encode::tonemap_chain(hdr));
        }
        let speed = item.edit.speed();
        let ctx = self.ctx.clone();
        let worker = encode::spawn(tools, self.caps.clone(), spec, move || ctx.request_repaint());
        self.sample = Some(SampleRun { item_id: item.id, worker, src_start: start, speed, before_vf });
        self.sample_status = "Encoding a 3-second sample…".to_string();
    }

    fn handle_sample(&mut self, msg: WorkerMsg) {
        let Some(item_id) = self.sample.as_ref().map(|s| s.item_id) else { return };
        match msg {
            WorkerMsg::Progress { fraction, .. } => self.sample_status = format!("Encoding a sample… {:.0}%", fraction * 100.0),
            WorkerMsg::Status(_) => {}
            WorkerMsg::Done { path, size, quality, .. } => {
                let run = self.sample.take().expect("checked above");
                let q = quality.map(|q| format!(" • SSIM {q:.3} ({})", encode::quality_label(q).to_lowercase())).unwrap_or_default();
                let summary = format!("{:.0} KiB{q}", size as f64 / 1024.0);
                self.sample_status.clear();
                if self.selected == Some(item_id) {
                    self.open_compare(run, path, summary);
                }
            }
            WorkerMsg::Failed(e) => {
                self.sample = None;
                self.sample_status = format!("The sample failed: {}", e.lines().last().unwrap_or(""));
            }
            WorkerMsg::Cancelled => {
                self.sample = None;
                self.sample_status.clear();
            }
        }
    }

    fn show_sample(&mut self, path: PathBuf) {
        let Some(tools) = &self.tools else { return };
        let Ok(info) = ffmpeg::probe(tools, &path) else { return };
        if info.width == 0 {
            return;
        }
        let (width, height) = MediaSource::preview_size(info.width, info.height);
        let source = MediaSource {
            path: path.clone(),
            duration: info.duration,
            fps: info.fps,
            video_index: info.video_index,
            audio_tracks: info.audio_tracks,
            mix_audio: false,
            track_volumes: Vec::new(),
            width,
            height,
        };
        if let Some(p) = &mut self.player {
            p.load(source, 0.0);
            p.play();
        }
        self.showing_sample = Some(path);
    }

    /// Shows the finished sample next to the original, at the middle of the sample.
    fn open_compare(&mut self, run: SampleRun, sample: PathBuf, summary: String) {
        let (Some(tools), Some(index)) = (&self.tools, self.index_of(run.item_id)) else { return };
        let Ok(info) = ffmpeg::probe(tools, &sample) else {
            self.sample_status = "Couldn't read the encoded sample.".to_string();
            return;
        };
        let item = &self.items[index];
        let (cw, ch) = match item.edit.effective_crop(item.info.width, item.info.height) {
            Some(c) => (c.w, c.h),
            None => (item.info.width, item.info.height),
        };
        let (width, height) = services::compare_size(cw, ch, 1920);
        let mut before_vf = run.before_vf;
        before_vf.push(format!("scale={width}:{height}:flags=lanczos"));
        if let Some(p) = &mut self.player {
            p.pause();
        }
        self.showing_sample = None;
        self.compare = Some(Compare {
            sample,
            source: item.path.clone(),
            duration: info.duration,
            fps: info.fps.max(1.0),
            width,
            height,
            src_start: run.src_start,
            speed: run.speed,
            before_vf: before_vf.join(","),
            t: info.duration / 2.0,
            before: None,
            after: None,
            loading: None,
            wanted: None,
            summary,
            zoom: self.compare.as_ref().map_or(1.0, |c| c.zoom),
            center: self.compare.as_ref().map_or(egui::vec2(0.5, 0.5), |c| c.center),
        });
        self.load_compare_frames();
    }

    /// Moves the comparison to sample time `t`, snapped to a frame.
    fn compare_seek(&mut self, t: f64) {
        let Some(c) = &mut self.compare else { return };
        let frame = 1.0 / c.fps;
        let last = (c.duration - frame).max(0.0);
        c.t = ((t / frame).round() * frame).clamp(0.0, last);
        self.load_compare_frames();
    }

    fn load_compare_frames(&mut self) {
        let (Some(c), Some(tools)) = (&mut self.compare, &self.tools) else { return };
        if c.loading.is_some() {
            c.wanted = Some(c.t);
            return;
        }
        let (tools, ctx) = (tools.clone(), self.ctx.clone());
        let (source, sample, before_vf) = (c.source.clone(), c.sample.clone(), c.before_vf.clone());
        let (w, h, t) = (c.width, c.height, c.t);
        let src_t = c.src_start + t * c.speed;
        let (tx, rx) = mpsc::channel();
        c.loading = Some(rx);
        thread::spawn(move || {
            let after_vf = format!("scale={w}:{h}:flags=lanczos");
            let before = services::grab_frame(&tools, &source, src_t, &before_vf, w, h);
            let after = services::grab_frame(&tools, &sample, t, &after_vf, w, h);
            let _ = tx.send(before.zip(after));
            ctx.request_repaint();
        });
    }

    fn poll_compare(&mut self) {
        let Some(c) = &mut self.compare else { return };
        let Some(rx) = &c.loading else { return };
        match rx.try_recv() {
            Ok(frames) => {
                c.loading = None;
                if let Some((before, after)) = frames {
                    let size = [c.width, c.height];
                    let opts = egui::TextureOptions::LINEAR;
                    let b = egui::ColorImage::from_rgba_unmultiplied(size, &before);
                    let a = egui::ColorImage::from_rgba_unmultiplied(size, &after);
                    match (&mut c.before, &mut c.after) {
                        (Some(bt), Some(at)) => {
                            bt.set(b, opts);
                            at.set(a, opts);
                        }
                        _ => {
                            c.before = Some(self.ctx.load_texture("compare-before", b, opts));
                            c.after = Some(self.ctx.load_texture("compare-after", a, opts));
                        }
                    }
                }
                if let Some(t) = c.wanted.take() {
                    c.t = t;
                    self.load_compare_frames();
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => c.loading = None,
        }
    }

    /// Plays the encoded sample instead of comparing frames.
    fn play_compared_sample(&mut self) {
        if let Some(path) = self.compare.as_ref().map(|c| c.sample.clone()) {
            self.show_sample(path);
        }
    }

    /// From playing the sample back to the frame comparison.
    fn back_to_compare(&mut self) {
        self.showing_sample = None;
        if let Some(p) = &mut self.player {
            p.pause();
        }
    }

    fn close_compare(&mut self) {
        self.compare = None;
        self.back_to_source();
    }

    fn back_to_source(&mut self) {
        self.showing_sample = None;
        if let Some(i) = self.selected_index() {
            let (src, at) = (self.items[i].media_source(), self.items[i].start);
            if let Some(p) = &mut self.player {
                p.load(src, at);
            }
        }
    }

    // ========================================================
    // AFTER ENCODING: CLIPBOARD, WEBHOOK
    // ========================================================

    fn after_output(&mut self, path: &Path) {
        if self.persisted.copy_to_clipboard {
            self.copy_to_clipboard(path);
        }
        if self.persisted.webhook_enabled && services::webhook_url_ok(&self.persisted.webhook_url) {
            self.post_to_discord(path);
        }
    }

    fn copy_to_clipboard(&mut self, path: &Path) {
        match services::copy_files(&mut self.clipboard, &[path.to_path_buf()]) {
            Ok(()) => self.status_text = format!("Copied {} to the clipboard. Paste it into Discord.", util::file_name_lossy(path)),
            Err(e) => self.status_text = format!("Could not copy to the clipboard: {e}"),
        }
    }

    fn post_to_discord(&mut self, path: &Path) {
        let (url, file, tx, ctx) = (self.persisted.webhook_url.clone(), path.to_path_buf(), self.bg_tx.clone(), self.ctx.clone());
        self.status_text = format!("Posting {} to Discord…", util::file_name_lossy(path));
        thread::spawn(move || {
            let msg = match services::post_to_webhook(&url, &file, "") {
                Ok(()) => BgMsg::Status(format!("Posted {} to Discord.", util::file_name_lossy(&file))),
                Err(e) => BgMsg::Error("Couldn't post to Discord".to_string(), e),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    // ========================================================
    // PRESETS
    // ========================================================

    fn save_preset(&mut self) {
        let Some(i) = self.selected_index() else { return };
        let name = self.preset_name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let preset = Preset {
            name: name.clone(),
            settings: self.items[i].settings,
            size_preset: self.persisted.size_preset,
            custom_mib: self.persisted.custom_mib,
        };
        match self.persisted.presets.iter_mut().find(|p| p.name == name) {
            Some(existing) => *existing = preset,
            None => self.persisted.presets.push(preset),
        }
        self.preset_name.clear();
        self.status_text = format!("Saved preset \"{name}\".");
    }

    fn apply_preset(&mut self, index: usize) {
        let (Some(i), Some(preset)) = (self.selected_index(), self.persisted.presets.get(index).cloned()) else { return };
        let (codec, _) = self.caps.resolve_codec(preset.settings.codec);
        let settings = EncodeSettings { codec, ..preset.settings };
        self.apply_settings(i, settings);
        self.remembered = settings;
        self.persisted.size_preset = preset.size_preset;
        self.persisted.custom_mib = preset.custom_mib;
        self.reload_preview();
        self.status_text = format!("Applied preset \"{}\".", preset.name);
    }

    fn dialogs_ui(&mut self, ctx: &egui::Context) {
        let p = self.palette();
        let Some(dialog) = self.dialogs.front() else { return };
        let mut chosen: Option<usize> = None;
        egui::Modal::new(Id::new("dialog")).show(ctx, |ui| {
            ui.set_max_width(480.0);
            let color = match dialog.kind {
                DialogKind::Info => p.text,
                DialogKind::Warning => p.accent_ink,
                DialogKind::Error => p.bad,
            };
            ui.label(RichText::new(&dialog.title).size(18.0).strong().color(color));
            ui.add_space(6.0);
            ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                ui.label(RichText::new(&dialog.body).color(p.text));
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                for (i, (label, _)) in dialog.buttons.iter().enumerate() {
                    let button = if i == 0 {
                        Button::new(RichText::new(label).color(p.on_accent)).fill(p.accent)
                    } else {
                        Button::new(label)
                    };
                    if ui.add(button).clicked() {
                        chosen = Some(i);
                    }
                }
            });
        });
        if chosen.is_none() && ctx.input(|i| i.key_pressed(Key::Escape)) {
            chosen = Some(dialog.buttons.len().saturating_sub(1));
        }
        if chosen.is_none() && ctx.input(|i| i.key_pressed(Key::Enter)) {
            chosen = Some(0);
        }
        if let Some(i) = chosen {
            if let Some(mut dialog) = self.dialogs.pop_front() {
                let action = dialog.buttons.swap_remove(i).1;
                match action {
                    DialogAction::Close => {}
                    DialogAction::EncodeAll => self.begin_run(true),
                    DialogAction::EncodeUnfinished => self.begin_run(false),
                    DialogAction::OpenFolder(dir) => {
                        let _ = open::that_detached(dir);
                    }
                }
            }
        }
    }

    fn drop_overlay(&self, ctx: &egui::Context) {
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if !hovering {
            return;
        }
        let p = self.palette();
        let screen = ctx.content_rect();
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::new("drop-overlay")));
        painter.rect_filled(screen, 0.0, Color32::from_black_alpha(170));
        let inner = screen.shrink(24.0);
        painter.rect_stroke(inner, 12.0, Stroke::new(2.0, p.accent), StrokeKind::Inside);
        painter.text(
            screen.center(),
            Align2::CENTER_CENTER,
            "Drop to add to the queue",
            FontId::proportional(24.0),
            Color32::WHITE,
        );
    }

    fn missing_ui(&self, ctx: &egui::Context) {
        let p = self.palette();
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(60.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("FFmpeg is needed").heading().color(p.text));
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("Missing required programs: {}", self.missing.join(", "))).color(p.bad),
                );
                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Install FFmpeg (it includes ffprobe) and restart the app. On Linux use your package \
                         manager, on macOS `brew install ffmpeg`, on Windows `winget install ffmpeg`. You can also \
                         put ffmpeg and ffprobe next to this program.",
                    )
                    .color(p.weak),
                );
            });
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.missing.is_empty() {
            self.missing_ui(ctx);
            return;
        }

        self.poll();
        self.handle_dropped_files(ctx);
        self.handle_shortcuts(ctx);
        if let Some(player) = &mut self.player {
            if player.update(ctx) {
                ctx.request_repaint();
            }
        }

        self.layout(ctx);
        self.dialogs_ui(ctx);
        self.drop_overlay(ctx);
        self.track_undo(ctx);

        if self.processing
            || !self.probing.is_empty()
            || self.hardware_rx.is_some()
            || self.sample.is_some()
            || self.compare.as_ref().is_some_and(|c| c.loading.is_some())
            || self.watcher.is_some()
            || self.items.iter().any(|i| i.thumbs.is_empty() || (i.waveform.is_none() && i.info.audio_tracks > 0))
        {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.commit_default_dir();
        eframe::set_value(storage, config::KEY_SETTINGS, &self.persisted);
        eframe::set_value(storage, config::KEY_ENCODE, &self.remembered);
        let queue: Vec<SavedItem> =
            if self.persisted.restore_queue { self.items.iter().map(Item::saved).collect() } else { Vec::new() };
        eframe::set_value(storage, config::KEY_QUEUE, &queue);
    }

    fn auto_save_interval(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(w) = &self.worker {
            w.cancel.cancel();
        }
        if let Some(run) = &self.sample {
            run.worker.cancel.cancel();
        }
        self.player = None;
    }
}

// ============================================================
// HELPERS
// ============================================================

fn codec_info(codec: Codec, speed: Speed, caps: &Caps) -> String {
    let x26x = match speed {
        Speed::Faster => "medium",
        Speed::Balanced => "slow",
        Speed::Best => "slower",
    };
    match (codec.backend, codec.family) {
        (Backend::Software, Family::H265) => format!(
            "H.265 / HEVC: great quality per MB. Uses libx265 {x26x} preset with a real two-pass encode. \
             Some Discord viewers, mainly on Linux, in Firefox or on older devices, can't play HEVC inline \
             and have to download it."
        ),
        (Backend::Software, Family::H264) => format!(
            "H.264 / AVC: better compatibility but normally needs more bitrate for the same visual quality. \
             Plays inline for everyone on Discord. Uses libx264 {x26x} preset with a two-pass encode."
        ),
        (Backend::Software, Family::Av1) => {
            let encoder = caps.av1.map(Av1Encoder::name).unwrap_or("no AV1 encoder");
            format!(
                "AV1: excellent compression efficiency, but encoding is slower. Using {encoder}. The sharpest \
                 option at a given size; plays inline in the Discord desktop app and browsers, though some \
                 older phones can't play it."
            )
        }
        (backend, _) => {
            let gpu = caps.gpu_for(codec);
            let encoder = gpu.map(|g| g.encoder.as_str()).unwrap_or("a GPU encoder");
            let tuning = match gpu {
                Some(g) if !g.tuned => " Your driver didn't accept the extra quality settings, so the basic ones are used.",
                _ => "",
            };
            format!(
                "{}: encodes on your {} through {} ({encoder}), usually several times faster than the CPU \
                 encoders. It needs more bitrate for the same quality, so short size limits look softer than with \
                 the CPU encoders.{tuning}",
                codec.family_label(),
                caps.engine_name(codec.engine()),
                backend.api_name()
            )
        }
    }
}

fn reveal_in_folder(path: &Path) {
    let folder = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
    };
    #[cfg(target_os = "windows")]
    {
        if path.is_file() {
            let mut arg = std::ffi::OsString::from("/select,");
            arg.push(path.as_os_str());
            if std::process::Command::new("explorer").arg(arg).spawn().is_ok() {
                return;
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if path.is_file() && std::process::Command::new("open").arg("-R").arg(path).spawn().is_ok() {
            return;
        }
    }
    let _ = open::that_detached(folder);
}
