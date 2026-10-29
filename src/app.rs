//! The window. Layout: defaults at the top, the queue on the left,
//! preview, trim, encoding and output for the selected video on the right, and progress
//! with the compress button at the bottom.

use crate::encode::{
    self, AudioMode, Backend, Codec, EncodeSettings, Family, FpsMode, JobSpec, ResolutionMode, SizePreset,
    Speed, TrackMode, Worker, WorkerMsg,
};
use crate::ffmpeg::{self, Av1Encoder, Caps, GpuEncoder, MediaInfo, Tools};
use crate::player::{MediaSource, Player};
use crate::theme::{self, Palette};
use crate::util;
use eframe::egui::{
    self, Align, Align2, Button, Color32, ComboBox, CursorIcon, DragValue, FontId, Frame, Id, Key, Layout,
    Margin, Modifiers, RichText, ScrollArea, Sense, Stroke, StrokeKind, TextEdit, Ui, Vec2,
};
use eframe::epaint::text::{LayoutJob, TextFormat, TextWrapping};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

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
    last_output: Option<PathBuf>,
    error: Option<String>,
}

impl Item {
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
            width,
            height,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Persisted {
    default_output_dir: String,
    size_preset: SizePreset,
    custom_mib: f64,
    volume: f32,
    muted: bool,
    last_add_dir: Option<String>,
}

impl Default for Persisted {
    fn default() -> Self {
        Persisted {
            default_output_dir: util::default_videos_dir(),
            size_preset: SizePreset::Discord10,
            custom_mib: 9.5,
            volume: 1.0,
            muted: false,
            last_add_dir: None,
        }
    }
}

impl Persisted {
    fn target_mib(&self) -> f64 {
        self.size_preset.target_mib(self.custom_mib)
    }

    fn target_bytes(&self) -> u64 {
        (self.target_mib() * 1024.0 * 1024.0) as u64
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
    Item(u64, PathBuf, Result<MediaInfo, String>),
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
    default_dir_buf: String,
    folder_buf: String,
    name_buf: String,
    dialogs: VecDeque<Dialog>,
    drag: Option<DragTarget>,
    resume_after_drag: bool,
    view_height: f32,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        tools: Result<Tools, Vec<&'static str>>,
        initial_files: Vec<PathBuf>,
    ) -> App {
        theme::apply(&cc.egui_ctx);
        let persisted: Persisted = cc
            .storage
            .and_then(|s| eframe::get_value(s, "settings"))
            .unwrap_or_default();
        let (probe_tx, probe_rx) = mpsc::channel();

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
            folder_buf: String::new(),
            name_buf: String::new(),
            dialogs: VecDeque::new(),
            drag: None,
            resume_after_drag: false,
            view_height: 600.0,
        };

        match tools {
            Ok(tools) => {
                let caps = Caps::detect(&tools);
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
                app.player = Some(Player::new(tools.clone(), app.persisted.volume, app.persisted.muted));
                app.tools = Some(tools);
            }
            Err(missing) => app.missing = missing,
        }
        if !initial_files.is_empty() {
            app.add_paths(initial_files);
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
                if !had_encoder {
                    if self.caps.has_any_encoder() {
                        let codec = self.caps.default_codec();
                        for item in &mut self.items {
                            item.settings.codec = codec;
                        }
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
                            let id = self.add_item(path, info);
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
            WorkerMsg::Done { path, size } => {
                let size_mib = util::mib(size);
                if let Some(i) = index {
                    self.items[i].status = Status::Done { size_mib };
                    self.items[i].last_output = Some(path.clone());
                    self.items[i].error = None;
                }
                if let Some(dir) = path.parent() {
                    if !self.run_folders.iter().any(|d| d == dir) {
                        self.run_folders.push(dir.to_path_buf());
                    }
                }
                self.run_done += 1;
                self.status_text = format!("Created: {} ({size_mib:.2} MiB)", util::file_name_lossy(&path));
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
        self.batches.insert(batch, Batch::default());
        if !self.processing {
            self.status_text = "Reading video info…".to_string();
        }
        let tx = self.probe_tx.clone();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            for path in batch_paths {
                let result = ffmpeg::probe(&tools, &path);
                let _ = tx.send(ProbeMsg::Item(batch, path, result));
                ctx.request_repaint();
            }
            let _ = tx.send(ProbeMsg::Finished(batch));
            ctx.request_repaint();
        });
    }

    fn add_item(&mut self, path: PathBuf, info: MediaInfo) -> u64 {
        let codec = self.caps.default_codec();
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(Item {
            id,
            output_name: encode::default_output_name(&path, codec),
            path,
            start: 0.0,
            end: info.duration,
            info,
            settings: EncodeSettings::new(codec),
            status: Status::Waiting,
            output_dir: self.persisted.default_output_dir.clone(),
            custom_name: false,
            last_output: None,
            error: None,
        });
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
            }
            None => {
                self.folder_buf.clear();
                self.name_buf.clear();
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
        item.output_name = encode::default_output_name(&item.path, item.settings.codec);
        item.custom_name = false;
        self.sync_buffers();
    }

    /// Applies new settings, keeping auto-generated filenames in step with the codec.
    fn apply_settings(&mut self, index: usize, settings: EncodeSettings) {
        let item = &mut self.items[index];
        let codec_changed = item.settings.codec.family != settings.codec.family;
        let tracks_changed = item.settings.tracks != settings.tracks;
        item.settings = settings;
        if codec_changed && !item.custom_name {
            item.output_name = encode::default_output_name(&item.path, settings.codec);
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
            if item.end <= item.start {
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

        let spec = JobSpec {
            item_id: item.id,
            input: item.path.clone(),
            output_dir: util::expand_tilde(&item.output_dir),
            output_name: util::sanitize_filename(&item.output_name),
            start: item.start,
            end: item.end,
            settings: item.settings,
            info: item.info.clone(),
            target_bytes: self.persisted.target_bytes(),
            target_mib: self.persisted.target_mib(),
        };
        self.worker = Some(encode::spawn(tools, self.caps.clone(), spec, self.ctx.clone()));
    }

    fn encoding_finished(&mut self) {
        self.processing = false;
        self.worker = None;
        self.progress = 1.0;
        self.progress_detail.clear();
        self.status_text = "All videos finished.".to_string();

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
        let Some(i) = self.selected_index() else { return };
        let (start, end) = (self.items[i].start, self.items[i].end);

        let (space, k_i, k_o, sl, sr, l, r, comma, period, home, end_key) = ctx.input_mut(|inp| {
            (
                inp.consume_key(Modifiers::NONE, Key::Space),
                inp.consume_key(Modifiers::NONE, Key::I),
                inp.consume_key(Modifiers::NONE, Key::O),
                inp.consume_key(Modifiers::SHIFT, Key::ArrowLeft),
                inp.consume_key(Modifiers::SHIFT, Key::ArrowRight),
                inp.consume_key(Modifiers::NONE, Key::ArrowLeft),
                inp.consume_key(Modifiers::NONE, Key::ArrowRight),
                inp.consume_key(Modifiers::NONE, Key::Comma),
                inp.consume_key(Modifiers::NONE, Key::Period),
                inp.consume_key(Modifiers::NONE, Key::Home),
                inp.consume_key(Modifiers::NONE, Key::End),
            )
        });
        if k_i {
            self.set_start_here();
        }
        if k_o {
            self.set_end_here();
        }
        let Some(player) = &mut self.player else { return };
        let pos = player.position();
        if space {
            player.toggle();
        }
        if sl {
            player.seek(pos - 1.0);
        }
        if sr {
            player.seek(pos + 1.0);
        }
        if l {
            player.seek(pos - 5.0);
        }
        if r {
            player.seek(pos + 5.0);
        }
        if comma {
            player.step_frame(-1.0);
        }
        if period {
            player.step_frame(1.0);
        }
        if home {
            player.seek(start);
        }
        if end_key {
            player.seek(end);
        }
    }

    // ========================================================
    // UI: HEADER
    // ========================================================

    fn header_ui(&mut self, ui: &mut Ui) {
        let p = self.palette();
        ui.horizontal(|ui| {
            ui.label(RichText::new("ClipCram").heading().strong().color(p.text));
            ui.label(RichText::new("squeezes clips under Discord's upload limit").color(p.accent_ink));
        });
        ui.label(
            RichText::new("Drag videos into the queue. Select a video to preview, trim, and configure its output.")
                .color(p.weak),
        );
        ui.add_space(4.0);

        section(ui, &p, "Defaults", |ui| {
            ui.horizontal(|ui| {
                ui.label("Encoded clips:");
                let width = (ui.available_width() - 250.0).max(160.0);
                let response = ui.add(
                    TextEdit::singleline(&mut self.default_dir_buf)
                        .hint_text("Folder for new encoded clips")
                        .desired_width(width),
                );
                if response.lost_focus() {
                    self.commit_default_dir();
                }
                if ui.button("Browse…").clicked() {
                    self.choose_default_folder();
                }
                let has_selection = self.selected_index().is_some();
                if ui
                    .add_enabled(has_selection, Button::new("Use for selected"))
                    .on_hover_text("Send the selected video's output to this folder")
                    .clicked()
                {
                    self.use_default_for_selected();
                }
            });
            ui.horizontal(|ui| {
                ui.label("Size limit:");
                ComboBox::from_id_salt("size-preset")
                    .selected_text(self.persisted.size_preset.label())
                    .width(200.0)
                    .show_ui(ui, |ui| {
                        for preset in SizePreset::ALL {
                            ui.selectable_value(&mut self.persisted.size_preset, preset, preset.label());
                        }
                    });
                if self.persisted.size_preset == SizePreset::Custom {
                    ui.add(
                        DragValue::new(&mut self.persisted.custom_mib)
                            .range(1.0..=4000.0)
                            .speed(0.1)
                            .fixed_decimals(1)
                            .suffix(" MiB"),
                    );
                }
                ui.label(
                    RichText::new(format!(
                        "Aiming for {:.1} MiB per video. Discord has changed its limits before, so use Custom if yours differs.",
                        self.persisted.target_mib()
                    ))
                    .color(p.weak),
                );
            });
        });
    }

    // ========================================================
    // UI: QUEUE
    // ========================================================

    fn queue_ui(&mut self, ui: &mut Ui) {
        let p = self.palette();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Video queue").strong().color(p.text));
            if !self.items.is_empty() {
                ui.label(RichText::new(format!("{}", self.items.len())).color(p.weak));
            }
            if !self.probing.is_empty() {
                ui.spinner();
            }
        });

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.horizontal(|ui| {
                if ui.button("Add videos").clicked() {
                    self.choose_files();
                }
                let can_edit = !self.processing;
                if ui
                    .add_enabled(can_edit && self.selected_index().is_some(), Button::new("Remove"))
                    .clicked()
                {
                    self.remove_selected();
                }
                if ui.add_enabled(can_edit && !self.items.is_empty(), Button::new("Clear")).clicked() {
                    self.clear_queue();
                }
            });
            ui.add_space(4.0);

            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    if self.items.is_empty() {
                        ui.add_space(24.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("No videos yet").color(p.text));
                            ui.label(
                                RichText::new("Drop video files or a folder anywhere in this window, or use Add videos.")
                                    .color(p.weak),
                            );
                        });
                        return;
                    }
                    let mut clicked: Option<u64> = None;
                    let mut remove: Option<u64> = None;
                    let mut reveal: Option<PathBuf> = None;
                    let progress = self.progress;
                    for item in &self.items {
                        let selected = Some(item.id) == self.selected;
                        let response = queue_row(ui, &p, item, selected, progress);
                        if response.clicked() {
                            clicked = Some(item.id);
                        }
                        let response = match &item.error {
                            Some(e) => response.on_hover_text(e),
                            None => response,
                        };
                        response.context_menu(|ui| {
                            if ui.button("Show source file").clicked() {
                                reveal = Some(item.path.clone());
                                ui.close();
                            }
                            if let Some(out) = &item.last_output {
                                if ui.button("Show encoded file").clicked() {
                                    reveal = Some(out.clone());
                                    ui.close();
                                }
                            }
                            if ui.add_enabled(!self.processing, Button::new("Remove")).clicked() {
                                remove = Some(item.id);
                                ui.close();
                            }
                        });
                        ui.add_space(4.0);
                    }
                    if let Some(id) = clicked {
                        self.select(Some(id));
                    }
                    if let Some(id) = remove {
                        self.select(Some(id));
                        self.remove_selected();
                    }
                    if let Some(path) = reveal {
                        reveal_in_folder(&path);
                    }
                });
            });
        });
    }

    // ========================================================
    // UI: PREVIEW, TIMELINE, CONTROLS
    // ========================================================

    fn preview_ui(&mut self, ui: &mut Ui) {
        let p = self.palette();
        let width = ui.available_width();
        let height = (width * 9.0 / 16.0).min(self.view_height * 0.55).clamp(200.0, 470.0);
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 8.0, p.video_bg);

        let has_item = self.selected_index().is_some();
        match self.player.as_ref().and_then(|pl| pl.texture()) {
            Some(texture) if has_item => {
                let size = texture.size_vec2();
                let scale = (rect.width() / size.x).min(rect.height() / size.y);
                let image_rect = egui::Rect::from_center_size(rect.center(), size * scale);
                painter.image(
                    texture.id(),
                    image_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            _ => {
                let text = if has_item {
                    "Loading preview…"
                } else {
                    "Drop videos here, or use Add videos"
                };
                painter.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    text,
                    FontId::proportional(16.0),
                    Color32::from_rgb(0x9A, 0xA1, 0xAD),
                );
            }
        }
        if has_item && response.clicked() {
            if let Some(player) = &mut self.player {
                player.toggle();
            }
        }
    }

    fn timeline_ui(&mut self, ui: &mut Ui) {
        let p = self.palette();
        let Some(index) = self.selected_index() else { return };
        let Some(player) = self.player.as_mut() else { return };
        let item = &mut self.items[index];
        let editable = item.status != Status::Encoding;
        let duration = item.info.duration.max(0.001);

        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 46.0), Sense::click_and_drag());
        let track = rect.shrink2(Vec2::new(8.0, 9.0));
        let x_of = |t: f64| track.left() + (t / duration).clamp(0.0, 1.0) as f32 * track.width();
        let t_of = |x: f32| ((x - track.left()) / track.width()).clamp(0.0, 1.0) as f64 * duration;

        let near = |x: f32, t: f64| (x - x_of(t)).abs() <= 8.0;
        if let Some(hover) = response.hover_pos() {
            if editable && (near(hover.x, item.start) || near(hover.x, item.end)) {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
            }
        }

        if response.drag_started() {
            if let Some(ptr) = response.interact_pointer_pos() {
                let ds = (ptr.x - x_of(item.start)).abs();
                let de = (ptr.x - x_of(item.end)).abs();
                let target = if editable && ds <= 8.0 && ds <= de {
                    DragTarget::Start
                } else if editable && de <= 8.0 {
                    DragTarget::End
                } else {
                    DragTarget::Playhead
                };
                self.resume_after_drag = target == DragTarget::Playhead && player.is_playing();
                player.pause();
                self.drag = Some(target);
            }
        }
        if response.dragged() {
            if let (Some(target), Some(ptr)) = (self.drag, response.interact_pointer_pos()) {
                let t = t_of(ptr.x);
                match target {
                    DragTarget::Playhead => player.seek(t),
                    DragTarget::Start => {
                        item.start = t.min(item.end - MIN_CLIP).max(0.0);
                        player.seek(item.start);
                    }
                    DragTarget::End => {
                        item.end = t.max(item.start + MIN_CLIP).min(item.info.duration);
                        player.seek(item.end);
                    }
                }
            }
        }
        if response.drag_stopped() {
            if self.resume_after_drag {
                player.play();
            }
            self.resume_after_drag = false;
            self.drag = None;
        } else if response.clicked() {
            if let Some(ptr) = response.interact_pointer_pos() {
                player.seek(t_of(ptr.x));
            }
        }

        // Paint.
        let painter = ui.painter_at(rect);
        painter.rect_filled(track, 6.0, p.input);
        painter.rect_stroke(track, 6.0, Stroke::new(1.0, p.stroke), StrokeKind::Inside);

        let (xs, xe) = (x_of(item.start), x_of(item.end));
        let clip = egui::Rect::from_x_y_ranges(xs..=xe, track.y_range());
        let fill = if ui.visuals().dark_mode {
            Color32::from_rgba_unmultiplied(240, 168, 58, 60)
        } else {
            Color32::from_rgba_unmultiplied(232, 158, 46, 80)
        };
        painter.rect_filled(clip, 0.0, fill);
        painter.hline(xs..=xe, track.top() + 1.0, Stroke::new(2.0, p.accent));
        painter.hline(xs..=xe, track.bottom() - 1.0, Stroke::new(2.0, p.accent));

        for x in [xs, xe] {
            let handle = egui::Rect::from_center_size(egui::pos2(x, track.center().y), Vec2::new(7.0, track.height() + 8.0));
            painter.rect_filled(handle, 2.0, if editable { p.accent } else { p.stroke_strong });
            for dy in [-4.0, 0.0, 4.0] {
                painter.hline(
                    (x - 1.5)..=(x + 1.5),
                    track.center().y + dy,
                    Stroke::new(1.0, p.on_accent),
                );
            }
        }

        let xp = x_of(player.position());
        painter.vline(xp, rect.top()..=rect.bottom(), Stroke::new(2.0, p.text));
        painter.circle_filled(egui::pos2(xp, rect.top() + 3.0), 4.0, p.text);

        if let Some(hover) = response.hover_pos() {
            let t = t_of(hover.x);
            let label = util::format_time_precise(t);
            let galley = painter.layout_no_wrap(label, FontId::proportional(12.0), p.text);
            let pos = egui::pos2(
                (hover.x - galley.size().x / 2.0).clamp(rect.left(), rect.right() - galley.size().x),
                rect.top() - galley.size().y - 2.0,
            );
            let bg = egui::Rect::from_min_size(pos, galley.size()).expand(3.0);
            let over = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, Id::new("timeline-hover")));
            over.rect_filled(bg, 4.0, p.surface);
            over.rect_stroke(bg, 4.0, Stroke::new(1.0, p.stroke), StrokeKind::Outside);
            over.galley(pos, galley, p.text);
        }
    }

    fn controls_ui(&mut self, ui: &mut Ui) {
        let p = self.palette();
        let Some(index) = self.selected_index() else { return };
        let (start, end, duration) = (self.items[index].start, self.items[index].end, self.items[index].info.duration);
        let Some(player) = self.player.as_mut() else { return };

        ui.horizontal(|ui| {
            let label = if player.is_playing() { "⏸ Pause" } else { "▶ Play" };
            if ui.add_sized([96.0, 28.0], Button::new(label)).clicked() {
                player.toggle();
            }
            if ui
                .button("Preview clip")
                .on_hover_text("Play from the start point and stop at the end point")
                .clicked()
            {
                player.play_range(start, end);
            }
            if ui.button("‹ Frame").on_hover_text("Step back one frame (comma)").clicked() {
                player.step_frame(-1.0);
            }
            if ui.button("Frame ›").on_hover_text("Step forward one frame (period)").clicked() {
                player.step_frame(1.0);
            }
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!(
                    "{} / {}",
                    util::format_time_precise(player.position()),
                    util::format_time(duration)
                ))
                .monospace()
                .color(p.text),
            );

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if player.has_audio_device() {
                    let mut volume = player.volume();
                    if ui
                        .add(egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false))
                        .on_hover_text("Preview volume")
                        .changed()
                    {
                        player.set_volume(volume);
                        self.persisted.volume = volume;
                    }
                    let muted = player.muted();
                    if ui.selectable_label(muted, "Mute").clicked() {
                        player.set_muted(!muted);
                        self.persisted.muted = !muted;
                    }
                } else {
                    ui.label(RichText::new("No audio output device").color(p.weak));
                }
            });
        });
    }

    // ========================================================
    // UI: TRIM / ENCODING / OUTPUT
    // ========================================================

    fn trim_ui(&mut self, ui: &mut Ui, index: usize) {
        let p = self.palette();
        let editable = self.items[index].status != Status::Encoding;
        let mut set_start = false;
        let mut set_end = false;
        section(ui, &p, "Trim selected video", |ui| {
            ui.add_enabled_ui(editable, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let item = &mut self.items[index];
                    let duration = item.info.duration;
                    ui.label("Start:");
                    let mut start = item.start;
                    let r = ui.add(
                        DragValue::new(&mut start)
                            .range(0.0..=(item.end - 0.01).max(0.0))
                            .speed(0.05)
                            .custom_formatter(|v, _| util::format_time_precise(v))
                            .custom_parser(util::parse_time),
                    );
                    if r.changed() {
                        item.start = start;
                    }
                    if ui.button("Set start here").on_hover_text("Shortcut: I").clicked() {
                        set_start = true;
                    }
                    ui.add_space(12.0);
                    ui.label("End:");
                    let mut end = item.end;
                    let r = ui.add(
                        DragValue::new(&mut end)
                            .range((item.start + 0.01).min(duration)..=duration)
                            .speed(0.05)
                            .custom_formatter(|v, _| util::format_time_precise(v))
                            .custom_parser(util::parse_time),
                    );
                    if r.changed() {
                        item.end = end;
                    }
                    if ui.button("Set end here").on_hover_text("Shortcut: O").clicked() {
                        set_end = true;
                    }
                    ui.add_space(12.0);
                    if ui.button("Use whole video").clicked() {
                        item.start = 0.0;
                        item.end = duration;
                    }
                });
            });

            let item = &self.items[index];
            let info = &item.info;
            let mut source = format!(
                "Source: {}×{}, {} fps, {} long",
                info.width,
                info.height,
                encode::format_fps(info.fps),
                util::format_time(info.duration)
            );
            if !info.video_codec.is_empty() {
                source.push_str(&format!(", {}", info.video_codec.to_uppercase()));
            }
            match info.audio_tracks {
                0 => source.push_str(", no audio"),
                1 => {}
                n => source.push_str(&format!(", {n} audio tracks")),
            }
            if let Some(hdr) = &info.hdr {
                source.push_str(if hdr.transfer == "arib-std-b67" { ", HDR (HLG)" } else { ", HDR (PQ)" });
            }
            ui.label(RichText::new(source).color(p.weak));
            ui.label(
                RichText::new(format!(
                    "Clip: {} to {} ({:.2} s)",
                    util::format_time_precise(item.start),
                    util::format_time_precise(item.end),
                    item.end - item.start
                ))
                .color(p.text),
            );
            ui.label(
                RichText::new(
                    "Drag the amber handles on the timeline to trim. Space plays or pauses, I and O set the \
                     start and end, arrow keys jump 5 seconds (1 with Shift), comma and period step one frame.",
                )
                .small()
                .color(p.weak),
            );
        });
        if set_start {
            self.set_start_here();
        }
        if set_end {
            self.set_end_here();
        }
    }

    fn encoding_ui(&mut self, ui: &mut Ui, index: usize) {
        let p = self.palette();
        let caps = self.caps.clone();
        let editable = self.items[index].status != Status::Encoding;
        let mut new_settings = self.items[index].settings;
        let mut apply_all = false;
        let target_bytes = self.persisted.target_bytes();
        let target_mib = self.persisted.target_mib();

        section(ui, &p, "Encoding", |ui| {
            let info = &self.items[index].info;
            ui.add_enabled_ui(editable, |ui| {
                egui::Grid::new("encoding-grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Codec:");
                        let mut options = caps.codec_options();
                        if !options.contains(&new_settings.codec) {
                            options.push(new_settings.codec);
                        }
                        ComboBox::from_id_salt("codec")
                            .selected_text(new_settings.codec.label())
                            .width(300.0)
                            .show_ui(ui, |ui| {
                                for codec in options {
                                    ui.selectable_value(&mut new_settings.codec, codec, codec.label());
                                }
                            });
                        ui.end_row();

                        ui.label("Resolution:");
                        ComboBox::from_id_salt("resolution")
                            .selected_text(new_settings.resolution.label())
                            .width(300.0)
                            .show_ui(ui, |ui| {
                                for mode in ResolutionMode::ALL {
                                    if let ResolutionMode::Short(s) = mode {
                                        if s >= info.width.min(info.height) {
                                            continue;
                                        }
                                    }
                                    ui.selectable_value(&mut new_settings.resolution, mode, mode.label());
                                }
                            })
                            .response
                            .on_hover_text(
                                "Auto picks the largest size that still looks clean at the bitrate the size limit \
                                 allows. Smaller looks sharper than a blocky full-size picture.",
                            );
                        ui.end_row();

                        ui.label("Frame rate:");
                        ComboBox::from_id_salt("fps")
                            .selected_text(new_settings.fps.label())
                            .width(300.0)
                            .show_ui(ui, |ui| {
                                for mode in FpsMode::ALL {
                                    ui.selectable_value(&mut new_settings.fps, mode, mode.label());
                                }
                            })
                            .response
                            .on_hover_text(
                                "Auto keeps the full frame rate unless that would push the picture below 480p, \
                                 in which case it halves the frame rate (60 to 30) to keep detail.",
                            );
                        ui.end_row();

                        ui.label("Speed:");
                        ComboBox::from_id_salt("speed")
                            .selected_text(new_settings.speed.label())
                            .width(300.0)
                            .show_ui(ui, |ui| {
                                for speed in Speed::ALL {
                                    ui.selectable_value(&mut new_settings.speed, speed, speed.label());
                                }
                            })
                            .response
                            .on_hover_text(
                                "Faster trades a little quality for speed. Best quality uses slower encoder settings \
                                 and, if the first result is well under the limit, re-runs the final pass to use \
                                 the leftover space.",
                            );
                        ui.end_row();

                        ui.label("Audio:");
                        ComboBox::from_id_salt("audio")
                            .selected_text(new_settings.audio.label())
                            .width(300.0)
                            .show_ui(ui, |ui| {
                                for mode in AudioMode::ALL {
                                    ui.selectable_value(&mut new_settings.audio, mode, mode.label());
                                }
                            });
                        ui.end_row();

                        if info.audio_tracks > 1 && new_settings.audio != AudioMode::Off {
                            ui.label("Audio tracks:");
                            ComboBox::from_id_salt("tracks")
                                .selected_text(new_settings.tracks.label())
                                .width(300.0)
                                .show_ui(ui, |ui| {
                                    for mode in TrackMode::ALL {
                                        ui.selectable_value(&mut new_settings.tracks, mode, mode.label());
                                    }
                                })
                                .response
                                .on_hover_text(
                                    "Discord only plays the first audio track. Mixing puts game audio and mic \
                                     (or any other tracks) together so everyone hears both.",
                                );
                            ui.end_row();
                        }

                        if info.hdr.is_some() {
                            ui.label("HDR:");
                            ui.add_enabled_ui(caps.tonemap, |ui| {
                                ui.checkbox(&mut new_settings.hdr_to_sdr, "Convert to SDR so colors look right on Discord")
                                    .on_disabled_hover_text("This FFmpeg build lacks the zscale filter needed for this.");
                            });
                            ui.end_row();
                        }
                    });
            });

            ui.add_space(4.0);
            ui.label(RichText::new(codec_info(new_settings.codec, new_settings.speed, &caps)).color(p.weak));

            let item = &self.items[index];
            let est = encode::estimate(&new_settings, &item.info, item.end - item.start, target_bytes, &caps);
            let audio = if est.audio_bps == 0 {
                "no audio".to_string()
            } else {
                format!("{} kbps audio", util::thousands(est.audio_bps / 1000))
            };
            ui.label(
                RichText::new(format!(
                    "Will encode at {}×{}, {} fps, about {} kbps video and {}, aiming for {:.1} MiB.",
                    est.plan.width,
                    est.plan.height,
                    encode::format_fps(est.plan.fps),
                    util::thousands(est.video_bps / 1000),
                    audio,
                    target_mib
                ))
                .color(p.text),
            );
            if est.tight {
                ui.label(
                    RichText::new(
                        "This clip is long for the size limit, so expect a blurry result. Trimming it, \
                         lowering the audio bitrate or choosing a bigger limit will help.",
                    )
                    .color(p.bad),
                );
            }
            if !caps.hardware_checked {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Checking for GPU encoders…").small().color(p.weak));
                });
            }
            if self.items.len() > 1 && ui.button("Apply these settings to all videos").clicked() {
                apply_all = true;
            }
        });

        if new_settings != self.items[index].settings && editable {
            self.apply_settings(index, new_settings);
        }
        if apply_all {
            self.apply_to_all();
        }
    }

    fn output_ui(&mut self, ui: &mut Ui, index: usize) {
        let p = self.palette();
        let editable = self.items[index].status != Status::Encoding;
        let mut browse = false;
        let mut reset = false;
        let mut folder_done = false;
        let mut name_done = false;
        let mut reveal: Option<PathBuf> = None;

        section(ui, &p, "Output for selected video", |ui| {
            ui.add_enabled_ui(editable, |ui| {
                let label_w = 70.0;
                let button_w = 100.0;
                let spacing = ui.spacing().item_spacing.x;
                let field_w = (ui.available_width() - label_w - button_w - spacing * 2.0).max(160.0);
                ui.horizontal(|ui| {
                    ui.add_sized([label_w, 24.0], egui::Label::new("Folder:"));
                    let r = ui.add(
                        TextEdit::singleline(&mut self.folder_buf)
                            .hint_text("Output folder")
                            .desired_width(field_w),
                    );
                    folder_done = r.lost_focus();
                    browse = ui.add_sized([button_w, 24.0], Button::new("Browse…")).clicked();
                });
                ui.horizontal(|ui| {
                    ui.add_sized([label_w, 24.0], egui::Label::new("Filename:"));
                    let r = ui.add(
                        TextEdit::singleline(&mut self.name_buf)
                            .hint_text("output.mp4")
                            .desired_width(field_w),
                    );
                    name_done = r.lost_focus();
                    reset = ui.add_sized([button_w, 24.0], Button::new("Reset name")).clicked();
                });
            });
            let item = &self.items[index];
            ui.label(RichText::new(format!("Output: {}", item.output_path().display())).color(p.weak));
            if let Some(out) = &item.last_output {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("Last encoded: {}", util::file_name_lossy(out))).color(p.ok));
                    if ui.button("Show in folder").clicked() {
                        reveal = Some(out.clone());
                    }
                });
            }
            if let (Status::Error, Some(err)) = (&item.status, &item.error) {
                let first_line = err.lines().last().unwrap_or("");
                ui.label(RichText::new(format!("Last attempt failed: {first_line}")).color(p.bad));
            }
        });

        if folder_done {
            self.commit_output_folder();
        }
        if name_done {
            self.commit_output_name();
        }
        if browse {
            self.choose_output_folder();
        }
        if reset {
            self.reset_output_name();
        }
        if let Some(path) = reveal {
            reveal_in_folder(&path);
        }
    }

    // ========================================================
    // UI: FOOTER, DIALOGS, OVERLAY
    // ========================================================

    fn footer_ui(&mut self, ui: &mut Ui) {
        let p = self.palette();
        ui.label(RichText::new(&self.status_text).color(p.text));
        let progress_text = if self.progress_detail.is_empty() {
            format!("{:.0}%", self.progress * 100.0)
        } else {
            self.progress_detail.clone()
        };
        progress_bar(ui, &p, self.progress, &progress_text, self.processing || self.progress > 0.0);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let cancel_width = if self.processing { 120.0 } else { 0.0 };
            let width = ui.available_width() - cancel_width;
            let button = Button::new(
                RichText::new("Compress all videos").size(17.0).strong().color(p.on_accent),
            )
            .fill(p.accent)
            .stroke(Stroke::NONE);
            if ui
                .add_enabled_ui(!self.processing, |ui| ui.add_sized([width, 46.0], button))
                .inner
                .clicked()
            {
                self.start_queue();
            }
            if self.processing && ui.add_sized([110.0, 46.0], Button::new("Cancel")).clicked() {
                self.cancel_encoding();
            }
        });
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

        let p = self.palette();
        egui::TopBottomPanel::top("header")
            .frame(Frame::new().fill(p.base).inner_margin(Margin { left: 16, right: 16, top: 12, bottom: 8 }))
            .show(ctx, |ui| self.header_ui(ui));

        egui::TopBottomPanel::bottom("footer")
            .frame(Frame::new().fill(p.base).inner_margin(Margin { left: 16, right: 16, top: 8, bottom: 14 }))
            .show(ctx, |ui| self.footer_ui(ui));

        egui::SidePanel::left("queue")
            .resizable(true)
            .default_width(390.0)
            .width_range(300.0..=620.0)
            .frame(Frame::new().fill(p.base).inner_margin(Margin { left: 16, right: 10, top: 6, bottom: 6 }))
            .show(ctx, |ui| self.queue_ui(ui));

        egui::CentralPanel::default()
            .frame(Frame::new().fill(p.base).inner_margin(Margin { left: 10, right: 16, top: 6, bottom: 6 }))
            .show(ctx, |ui| {
                self.view_height = ui.available_height();
                ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    self.preview_ui(ui);
                    ui.add_space(4.0);
                    self.timeline_ui(ui);
                    self.controls_ui(ui);
                    if let Some(index) = self.selected_index() {
                        ui.add_space(6.0);
                        self.trim_ui(ui, index);
                        ui.add_space(6.0);
                        self.encoding_ui(ui, index);
                        ui.add_space(6.0);
                        self.output_ui(ui, index);
                    }
                });
            });

        self.dialogs_ui(ctx);
        self.drop_overlay(ctx);

        if self.processing || !self.probing.is_empty() || self.hardware_rx.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.commit_default_dir();
        eframe::set_value(storage, "settings", &self.persisted);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(w) = &self.worker {
            w.cancel.cancel();
        }
        self.player = None;
    }
}

// ============================================================
// HELPERS
// ============================================================

fn section<R>(ui: &mut Ui, p: &Palette, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.stroke))
        .corner_radius(8.0)
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).strong().color(p.text));
            ui.add_space(2.0);
            add(ui)
        })
        .inner
}

fn progress_bar(ui: &mut Ui, p: &Palette, fraction: f32, text: &str, show_text: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 6.0, p.input);
    let fraction = fraction.clamp(0.0, 1.0);
    if fraction > 0.0 {
        let fill = egui::Rect::from_min_size(rect.min, Vec2::new(rect.width() * fraction, rect.height()));
        painter.rect_filled(fill, 6.0, p.accent);
    }
    if show_text {
        let galley = painter.layout_no_wrap(text.to_string(), FontId::proportional(12.5), p.text);
        let pos = rect.center() - galley.size() / 2.0;
        let bg = egui::Rect::from_min_size(pos, galley.size()).expand2(Vec2::new(6.0, 1.0));
        painter.rect_filled(bg, 4.0, p.surface.gamma_multiply(0.85));
        painter.galley(pos, galley, p.text);
    }
}

fn queue_row(ui: &mut Ui, p: &Palette, item: &Item, selected: bool, progress: f32) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 70.0), Sense::click());
    let painter = ui.painter_at(rect);
    let bg = if selected {
        p.button_hover
    } else if response.hovered() {
        p.button
    } else {
        p.surface
    };
    let border = if selected { p.accent } else { p.stroke };
    painter.rect(rect, 8.0, bg, Stroke::new(1.0, border), StrokeKind::Inside);

    let left = rect.left() + 12.0;
    let max_w = rect.width() - 24.0;
    let status_color = item.status.color(p);

    let mut line1 = LayoutJob::default();
    line1.append(
        &item.status.text(),
        0.0,
        TextFormat { font_id: FontId::proportional(13.0), color: status_color, ..Default::default() },
    );
    line1.append(
        &format!("   {}", item.name()),
        0.0,
        TextFormat { font_id: FontId::proportional(14.0), color: p.text, ..Default::default() },
    );
    line1.wrap = TextWrapping::truncate_at_width(max_w);
    let g1 = painter.layout_job(line1);

    let mut line2 = LayoutJob::default();
    line2.append(
        &format!(
            "{} – {}   •   {}   •   {:.1} s",
            util::format_time(item.start),
            util::format_time(item.end),
            item.settings.codec.short(),
            item.end - item.start
        ),
        0.0,
        TextFormat { font_id: FontId::proportional(12.5), color: p.weak, ..Default::default() },
    );
    line2.wrap = TextWrapping::truncate_at_width(max_w);
    let g2 = painter.layout_job(line2);

    let mut line3 = LayoutJob::default();
    line3.append(
        &format!("Saves as {}", item.output_name),
        0.0,
        TextFormat { font_id: FontId::proportional(12.5), color: p.weak, ..Default::default() },
    );
    line3.wrap = TextWrapping::truncate_at_width(max_w);
    let g3 = painter.layout_job(line3);

    painter.galley(egui::pos2(left, rect.top() + 8.0), g1, p.text);
    painter.galley(egui::pos2(left, rect.top() + 29.0), g2, p.weak);
    painter.galley(egui::pos2(left, rect.top() + 47.0), g3, p.weak);

    if item.status == Status::Encoding {
        let bar = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 1.0, rect.bottom() - 4.0),
            Vec2::new((rect.width() - 2.0) * progress.clamp(0.0, 1.0), 3.0),
        );
        painter.rect_filled(bar, 1.5, p.accent);
    }
    response
}

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
            let encoder = caps.gpu_for(codec).map(|g| g.encoder.as_str()).unwrap_or("a GPU encoder");
            let vendor = if backend == Backend::Vaapi { "graphics card (VA-API)".to_string() } else { format!("{} GPU", backend.vendor()) };
            format!(
                "{}: encodes on your {vendor} with {encoder}, usually several times faster than the CPU encoders. \
                 It needs more bitrate for the same quality, so short size limits look softer than with the CPU \
                 encoders.",
                codec.family_label()
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
