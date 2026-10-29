//! The preview player. FFmpeg (already required for encoding) decodes small RGBA frames and
//! PCM audio through pipes, so the player handles every format FFmpeg does with no extra
//! libraries. Audio drives the clock so picture and sound stay in sync.

use crate::ffmpeg::{self, Tools};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// ============================================================
// AUDIO OUTPUT
// ============================================================

struct AudioState {
    samples: VecDeque<f32>,
    generation: u64,
    played_frames: u64,
    active: bool,
    eof: bool,
}

struct AudioShared {
    state: Mutex<AudioState>,
    volume: AtomicU32,
}

impl AudioShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, AudioState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

pub struct AudioOut {
    _stream: cpal::Stream,
    shared: Arc<AudioShared>,
    sample_rate: u32,
}

impl AudioOut {
    /// Opens the default output device. Returns None when there's no usable device;
    /// the player then runs silently on a wall clock.
    pub fn new() -> Option<AudioOut> {
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        let supported = device.default_output_config().ok()?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let shared = Arc::new(AudioShared {
            state: Mutex::new(AudioState {
                samples: VecDeque::new(),
                generation: 0,
                played_frames: 0,
                active: false,
                eof: false,
            }),
            volume: AtomicU32::new(1.0f32.to_bits()),
        });

        let stream = match sample_format {
            cpal::SampleFormat::F32 => build_stream::<f32>(&device, &config, shared.clone()),
            cpal::SampleFormat::I16 => build_stream::<i16>(&device, &config, shared.clone()),
            cpal::SampleFormat::U16 => build_stream::<u16>(&device, &config, shared.clone()),
            cpal::SampleFormat::I32 => build_stream::<i32>(&device, &config, shared.clone()),
            cpal::SampleFormat::F64 => build_stream::<f64>(&device, &config, shared.clone()),
            cpal::SampleFormat::U8 => build_stream::<u8>(&device, &config, shared.clone()),
            cpal::SampleFormat::I8 => build_stream::<i8>(&device, &config, shared.clone()),
            _ => return None,
        }
        .ok()?;
        stream.play().ok()?;

        Some(AudioOut {
            _stream: stream,
            shared,
            sample_rate: config.sample_rate.0,
        })
    }

    fn begin(&self) -> u64 {
        let mut st = self.shared.lock();
        st.samples.clear();
        st.played_frames = 0;
        st.generation += 1;
        st.active = true;
        st.eof = false;
        st.generation
    }

    fn end(&self) {
        let mut st = self.shared.lock();
        st.samples.clear();
        st.active = false;
        st.generation += 1;
    }

    /// (seconds played, finished playing everything) for the given session.
    fn status(&self, generation: u64) -> Option<(f64, bool)> {
        let st = self.shared.lock();
        if st.generation != generation {
            return None;
        }
        Some((
            st.played_frames as f64 / self.sample_rate as f64,
            st.eof && st.samples.is_empty(),
        ))
    }

    pub fn set_volume(&self, volume: f32) {
        self.shared.volume.store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Arc<AudioShared>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = config.channels.max(1) as usize;
    device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let volume = f32::from_bits(shared.volume.load(Ordering::Relaxed));
            let mut st = shared.lock();
            for frame in data.chunks_mut(channels) {
                let (l, r) = if st.active && st.samples.len() >= 2 {
                    let l = st.samples.pop_front().unwrap_or(0.0);
                    let r = st.samples.pop_front().unwrap_or(0.0);
                    st.played_frames += 1;
                    (l * volume, r * volume)
                } else {
                    (0.0, 0.0)
                };
                if frame.len() == 1 {
                    frame[0] = T::from_sample((l + r) * 0.5);
                } else {
                    frame[0] = T::from_sample(l);
                    frame[1] = T::from_sample(r);
                    for sample in frame.iter_mut().skip(2) {
                        *sample = T::from_sample(0.0f32);
                    }
                }
            }
        },
        |err| eprintln!("audio output error: {err}"),
        None,
    )
}

fn audio_reader(mut stdout: ChildStdout, shared: Arc<AudioShared>, generation: u64, sample_rate: u32) {
    // Keep about 0.75 s buffered: enough to ride out hiccups, small enough to seek quickly.
    let max_samples = sample_rate as usize * 2 * 3 / 4;
    let mut buf = vec![0u8; 16 * 1024];
    let mut carry: Vec<u8> = Vec::new();
    loop {
        loop {
            let st = shared.lock();
            if st.generation != generation {
                return;
            }
            if st.samples.len() < max_samples {
                break;
            }
            drop(st);
            thread::sleep(Duration::from_millis(5));
        }
        let n = match stdout.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        carry.extend_from_slice(&buf[..n]);
        let usable = carry.len() / 4 * 4;
        {
            let mut st = shared.lock();
            if st.generation != generation {
                return;
            }
            st.samples.extend(
                carry[..usable]
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])),
            );
        }
        carry.drain(..usable);
    }
    let mut st = shared.lock();
    if st.generation == generation {
        st.eof = true;
    }
}

// ============================================================
// MEDIA
// ============================================================

#[derive(Clone, Debug, PartialEq)]
pub struct MediaSource {
    pub path: PathBuf,
    pub duration: f64,
    pub fps: f64,
    pub video_index: usize,
    pub audio_tracks: usize,
    pub mix_audio: bool,
    /// Preview frame size (small, to keep decoding cheap).
    pub width: usize,
    pub height: usize,
}

impl MediaSource {
    pub fn preview_size(display_w: u32, display_h: u32) -> (usize, usize) {
        let (box_w, box_h) = if display_w >= display_h { (960.0, 540.0) } else { (540.0, 960.0) };
        let scale = (box_w / display_w as f64).min(box_h / display_h as f64).min(1.0);
        let even = |v: f64| ((v.round() as usize) & !1).max(2);
        (even(display_w as f64 * scale), even(display_h as f64 * scale))
    }

    fn preview_fps(&self) -> f64 {
        if self.fps > 0.0 {
            self.fps.min(60.0)
        } else {
            30.0
        }
    }
}

struct Frame {
    pts: f64,
    rgba: Vec<u8>,
}

/// One running playback: an FFmpeg for video, another for audio.
struct Session {
    start: f64,
    children: Vec<Child>,
    rx: Receiver<Frame>,
    pending: Option<Frame>,
    video_done: bool,
    audio_generation: Option<u64>,
    wall: Option<(Instant, f64)>,
    last_clock: f64,
}

impl Session {
    fn clock(&mut self, audio: Option<&AudioOut>) -> f64 {
        if self.wall.is_none() {
            if let (Some(generation), Some(audio)) = (self.audio_generation, audio) {
                match audio.status(generation) {
                    Some((played, false)) => {
                        let now = self.start + played;
                        self.last_clock = now;
                        return now;
                    }
                    Some((played, true)) => {
                        // Audio ran out before the picture did; carry on with a wall clock.
                        let at = self.last_clock.max(self.start + played);
                        self.wall = Some((Instant::now(), at));
                    }
                    None => {
                        self.wall = Some((Instant::now(), self.last_clock.max(self.start)));
                    }
                }
            }
        }
        let (t0, p0) = *self.wall.get_or_insert((Instant::now(), self.start));
        let now = p0 + t0.elapsed().as_secs_f64();
        self.last_clock = now;
        now
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct StillRequest {
    rx: Receiver<Option<Vec<u8>>>,
    at: f64,
    retried: bool,
}

pub struct Player {
    tools: Tools,
    hwaccel: Option<String>,
    media: Option<MediaSource>,
    session: Option<Session>,
    still: Option<StillRequest>,
    wanted_still: Option<(f64, bool)>,
    texture: Option<TextureHandle>,
    position: f64,
    stop_at: Option<f64>,
    audio: Option<AudioOut>,
    volume: f32,
    muted: bool,
}

impl Player {
    pub fn new(tools: Tools, volume: f32, muted: bool) -> Player {
        let audio = AudioOut::new();
        let player = Player {
            tools,
            hwaccel: None,
            media: None,
            session: None,
            still: None,
            wanted_still: None,
            texture: None,
            position: 0.0,
            stop_at: None,
            audio,
            volume,
            muted,
        };
        player.apply_volume();
        player
    }

    pub fn set_hwaccel(&mut self, hwaccel: Option<String>) {
        self.hwaccel = hwaccel;
    }

    pub fn has_audio_device(&self) -> bool {
        self.audio.is_some()
    }

    pub fn load(&mut self, media: MediaSource, at: f64) {
        self.stop_session();
        self.stop_at = None;
        self.position = at.clamp(0.0, media.duration);
        self.media = Some(media);
        self.request_still(self.position);
    }

    pub fn unload(&mut self) {
        self.stop_session();
        self.media = None;
        self.texture = None;
        self.still = None;
        self.wanted_still = None;
        self.position = 0.0;
        self.stop_at = None;
    }

    pub fn is_playing(&self) -> bool {
        self.session.is_some()
    }

    pub fn position(&self) -> f64 {
        self.position
    }

    pub fn texture(&self) -> Option<&TextureHandle> {
        self.texture.as_ref()
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        self.apply_volume();
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        self.apply_volume();
    }

    fn apply_volume(&self) {
        if let Some(a) = &self.audio {
            a.set_volume(if self.muted { 0.0 } else { self.volume });
        }
    }

    pub fn play(&mut self) {
        let Some(media) = self.media.clone() else { return };
        if self.position >= media.duration - 0.05 {
            self.position = 0.0;
        }
        self.stop_at = None;
        self.start_session(self.position);
    }

    /// Plays from `start` and pauses at `end` (the "Preview clip" button).
    pub fn play_range(&mut self, start: f64, end: f64) {
        if self.media.is_none() {
            return;
        }
        self.position = start;
        self.start_session(start);
        self.stop_at = Some(end);
    }

    pub fn pause(&mut self) {
        if self.session.is_some() {
            self.stop_session();
        }
        self.stop_at = None;
    }

    pub fn toggle(&mut self) {
        if self.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Jumps to `t`. While playing, playback continues from there; while paused, the frame
    /// at `t` is fetched (requests are coalesced, so dragging stays responsive).
    pub fn seek(&mut self, t: f64) {
        let Some(media) = &self.media else { return };
        let t = t.clamp(0.0, media.duration);
        self.position = t;
        if self.session.is_some() {
            let stop_at = self.stop_at;
            self.start_session(t);
            self.stop_at = stop_at.filter(|&end| end > t);
        } else {
            self.request_still(t);
        }
    }

    /// Steps one frame forward or back (pauses first).
    pub fn step_frame(&mut self, direction: f64) {
        let Some(media) = &self.media else { return };
        let frame = 1.0 / media.fps.max(1.0);
        self.pause();
        self.seek(self.position + direction * frame);
    }

    fn stop_session(&mut self) {
        if let Some(audio) = &self.audio {
            if self.session.as_ref().is_some_and(|s| s.audio_generation.is_some()) {
                audio.end();
            }
        }
        self.session = None;
    }

    fn start_session(&mut self, at: f64) {
        self.stop_session();
        let Some(media) = self.media.clone() else { return };
        let fps = media.preview_fps();
        let mut children = Vec::new();

        // Video: small RGBA frames at a constant rate.
        let mut cmd = ffmpeg::command(&self.tools.ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-v", "error"]);
        if let Some(h) = &self.hwaccel {
            cmd.args(["-hwaccel", h]);
        }
        cmd.args(["-ss", &format!("{at:.3}"), "-i"])
            .arg(&media.path)
            .args(["-map", &format!("0:{}", media.video_index), "-an", "-sn"])
            .args([
                "-vf",
                &format!("fps={fps:.6},scale={}:{}:flags=fast_bilinear", media.width, media.height),
            ])
            .args(["-pix_fmt", "rgba", "-f", "rawvideo", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let (tx, rx) = mpsc::sync_channel::<Frame>(6);
        match cmd.spawn() {
            Ok(mut child) => {
                if let Some(stdout) = child.stdout.take() {
                    let (w, h) = (media.width, media.height);
                    thread::spawn(move || video_reader(stdout, tx, at, fps, w, h));
                }
                children.push(child);
            }
            Err(_) => return,
        }

        // Audio: interleaved stereo float at the device's rate.
        let mut audio_generation = None;
        if let Some(audio) = &self.audio {
            if media.audio_tracks > 0 {
                let mut cmd = ffmpeg::command(&self.tools.ffmpeg);
                cmd.args(["-hide_banner", "-nostdin", "-v", "error", "-ss", &format!("{at:.3}"), "-i"])
                    .arg(&media.path)
                    .arg("-vn");
                if media.mix_audio && media.audio_tracks > 1 {
                    let inputs: String = (0..media.audio_tracks).map(|i| format!("[0:a:{i}]")).collect();
                    cmd.args([
                        "-filter_complex",
                        &format!(
                            "{inputs}amix=inputs={}:duration=longest:normalize=0[a]",
                            media.audio_tracks
                        ),
                        "-map",
                        "[a]",
                    ]);
                } else {
                    cmd.args(["-map", "0:a:0"]);
                }
                cmd.args(["-ac", "2", "-ar", &audio.sample_rate.to_string(), "-f", "f32le", "-"])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null());
                if let Ok(mut child) = cmd.spawn() {
                    if let Some(stdout) = child.stdout.take() {
                        let generation = audio.begin();
                        let shared = audio.shared.clone();
                        let rate = audio.sample_rate;
                        thread::spawn(move || audio_reader(stdout, shared, generation, rate));
                        audio_generation = Some(generation);
                    }
                    children.push(child);
                }
            }
        }

        self.position = at;
        self.session = Some(Session {
            start: at,
            children,
            rx,
            pending: None,
            video_done: false,
            audio_generation,
            wall: None,
            last_clock: at,
        });
    }

    fn request_still(&mut self, at: f64) {
        self.wanted_still = Some((at, false));
    }

    fn launch_still(&mut self, at: f64, retried: bool) {
        let Some(media) = self.media.clone() else { return };
        let mut cmd = ffmpeg::command(&self.tools.ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-v", "error", "-ss", &format!("{at:.3}"), "-i"])
            .arg(&media.path)
            .args(["-map", &format!("0:{}", media.video_index), "-frames:v", "1", "-an", "-sn"])
            .args(["-vf", &format!("scale={}:{}:flags=bilinear", media.width, media.height)])
            .args(["-pix_fmt", "rgba", "-f", "rawvideo", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let expected = media.width * media.height * 4;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = cmd.output().ok().and_then(|o| {
                (o.stdout.len() >= expected).then(|| {
                    let mut v = o.stdout;
                    v.truncate(expected);
                    v
                })
            });
            let _ = tx.send(result);
        });
        self.still = Some(StillRequest { rx, at, retried });
    }

    fn show_frame(&mut self, ctx: &egui::Context, rgba: &[u8]) {
        let Some(media) = &self.media else { return };
        if rgba.len() != media.width * media.height * 4 {
            return;
        }
        let image = ColorImage::from_rgba_premultiplied([media.width, media.height], rgba);
        match &mut self.texture {
            Some(texture) => texture.set(image, TextureOptions::LINEAR),
            None => {
                self.texture = Some(ctx.load_texture("preview-frame", image, TextureOptions::LINEAR));
            }
        }
    }

    /// Call once per UI frame. Returns true while something is happening that needs repaints.
    pub fn update(&mut self, ctx: &egui::Context) -> bool {
        let mut busy = false;

        // Still frames for scrubbing while paused.
        if let Some(req) = &self.still {
            match req.rx.try_recv() {
                Ok(Some(rgba)) => {
                    self.still = None;
                    if self.session.is_none() {
                        self.show_frame(ctx, &rgba);
                    }
                }
                Ok(None) | Err(TryRecvError::Disconnected) => {
                    // Seeking right at the very end can return nothing; back off a little once.
                    let (at, retried) = (req.at, req.retried);
                    self.still = None;
                    if !retried && at > 0.0 && self.wanted_still.is_none() {
                        self.wanted_still = Some(((at - 1.0).max(0.0), true));
                    }
                }
                Err(TryRecvError::Empty) => busy = true,
            }
        }
        if self.still.is_none() {
            if let Some((at, retried)) = self.wanted_still.take() {
                self.launch_still(at, retried);
                busy = true;
            }
        }

        // Playback.
        let duration = self.media.as_ref().map(|m| m.duration).unwrap_or(0.0);
        let mut latest: Option<Frame> = None;
        let mut finished_at: Option<f64> = None;
        if let Some(session) = &mut self.session {
            busy = true;
            let now = session.clock(self.audio.as_ref());
            loop {
                if session.pending.is_none() {
                    match session.rx.try_recv() {
                        Ok(frame) => session.pending = Some(frame),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            session.video_done = true;
                            break;
                        }
                    }
                }
                let due = session.pending.as_ref().is_some_and(|f| f.pts <= now + 0.008);
                if due {
                    latest = session.pending.take();
                } else {
                    break;
                }
            }
            self.position = now.min(duration);

            if let Some(end) = self.stop_at {
                if now >= end {
                    finished_at = Some(end);
                }
            }
            if session.video_done && session.pending.is_none() {
                let audio_done = match (session.audio_generation, &self.audio) {
                    (Some(generation), Some(audio)) => {
                        audio.status(generation).map(|(_, done)| done).unwrap_or(true)
                    }
                    _ => true,
                };
                if audio_done || now >= duration {
                    finished_at = Some(now.min(duration));
                }
            }
        }
        if let Some(frame) = latest {
            self.show_frame(ctx, &frame.rgba);
        }
        if let Some(at) = finished_at {
            self.stop_session();
            self.stop_at = None;
            self.position = at;
        }
        busy
    }
}

fn video_reader(mut stdout: ChildStdout, tx: SyncSender<Frame>, start: f64, fps: f64, w: usize, h: usize) {
    let frame_bytes = w * h * 4;
    let mut n: u64 = 0;
    loop {
        let mut rgba = vec![0u8; frame_bytes];
        if stdout.read_exact(&mut rgba).is_err() {
            break;
        }
        let pts = start + n as f64 / fps;
        if tx.send(Frame { pts, rgba }).is_err() {
            break;
        }
        n += 1;
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop_session();
    }
}
