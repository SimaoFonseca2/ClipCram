//! Everything about turning one queue item into a file under the size limit:
//! settings, resolution/frame-rate planning, FFmpeg arguments, and the worker thread.

use crate::ffmpeg::{self, Av1Encoder, Caps, GpuEncoder, HdrInfo, MediaInfo, Tools, NULL_DEVICE};
use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

/// Lowest video bitrate ever used, however long the clip.
const MIN_VIDEO_BPS: u64 = 80_000;
/// Encoding attempts before giving up on fitting under the limit.
const MAX_ATTEMPTS: usize = 5;

// ============================================================
// CODECS
// ============================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Family {
    H264,
    H265,
    Av1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Backend {
    Software,
    Nvenc,
    Qsv,
    Vaapi,
    Amf,
    VideoToolbox,
}

impl Backend {
    pub fn vendor(self) -> &'static str {
        match self {
            Backend::Software => "CPU",
            Backend::Nvenc => "NVIDIA",
            Backend::Qsv => "Intel",
            Backend::Vaapi => "VA-API",
            Backend::Amf => "AMD",
            Backend::VideoToolbox => "Apple",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Codec {
    pub family: Family,
    pub backend: Backend,
}

impl Codec {
    pub const H265: Codec = Codec { family: Family::H265, backend: Backend::Software };
    pub const H264: Codec = Codec { family: Family::H264, backend: Backend::Software };
    pub const AV1: Codec = Codec { family: Family::Av1, backend: Backend::Software };

    pub fn is_gpu(self) -> bool {
        self.backend != Backend::Software
    }

    pub fn family_label(self) -> &'static str {
        match self.family {
            Family::H265 => "H.265 / HEVC",
            Family::H264 => "H.264 / AVC",
            Family::Av1 => "AV1",
        }
    }

    pub fn label(self) -> String {
        match self.backend {
            Backend::Software => self.family_label().to_string(),
            Backend::Vaapi => format!("{} on GPU via VA-API (fast)", self.family_label()),
            b => format!("{} on {} GPU (fast)", self.family_label(), b.vendor()),
        }
    }

    pub fn short(self) -> String {
        let name = match self.family {
            Family::H265 => "HEVC",
            Family::H264 => "H264",
            Family::Av1 => "AV1",
        };
        if self.is_gpu() {
            format!("{name} GPU")
        } else {
            name.to_string()
        }
    }

    /// Suffix added to output filenames, e.g. `clip_discord_h265.mp4`.
    pub fn file_suffix(self) -> &'static str {
        match self.family {
            Family::H265 => "_discord_h265.mp4",
            Family::H264 => "_discord_h264.mp4",
            Family::Av1 => "_discord_av1.mp4",
        }
    }
}

pub fn default_output_name(input: &Path, codec: Codec) -> String {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "video".to_string());
    format!("{stem}{}", codec.file_suffix())
}

// ============================================================
// SETTINGS
// ============================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolutionMode {
    Auto,
    Original,
    /// Fixed size by the short side (e.g. 720 for 1280×720 or 720×1280). Never upscales.
    Short(u32),
}

impl ResolutionMode {
    pub const ALL: [ResolutionMode; 8] = [
        ResolutionMode::Auto,
        ResolutionMode::Original,
        ResolutionMode::Short(1440),
        ResolutionMode::Short(1080),
        ResolutionMode::Short(720),
        ResolutionMode::Short(540),
        ResolutionMode::Short(480),
        ResolutionMode::Short(360),
    ];

    pub fn label(self) -> String {
        match self {
            ResolutionMode::Auto => "Auto — best quality".to_string(),
            ResolutionMode::Original => "Original".to_string(),
            ResolutionMode::Short(s) => format!("{s}p"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FpsMode {
    Auto,
    Original,
    Max60,
    Max30,
}

impl FpsMode {
    pub const ALL: [FpsMode; 4] = [FpsMode::Auto, FpsMode::Original, FpsMode::Max60, FpsMode::Max30];

    pub fn label(self) -> &'static str {
        match self {
            FpsMode::Auto => "Auto",
            FpsMode::Original => "Original",
            FpsMode::Max60 => "Up to 60 fps",
            FpsMode::Max30 => "Up to 30 fps",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Speed {
    Faster,
    Balanced,
    Best,
}

impl Speed {
    pub const ALL: [Speed; 3] = [Speed::Faster, Speed::Balanced, Speed::Best];

    pub fn label(self) -> &'static str {
        match self {
            Speed::Faster => "Faster",
            Speed::Balanced => "Balanced",
            Speed::Best => "Best quality",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioMode {
    Aac64,
    Aac96,
    Aac128,
    Aac160,
    Off,
}

impl AudioMode {
    pub const ALL: [AudioMode; 5] = [
        AudioMode::Aac96,
        AudioMode::Aac64,
        AudioMode::Aac128,
        AudioMode::Aac160,
        AudioMode::Off,
    ];

    pub fn kbps(self) -> u32 {
        match self {
            AudioMode::Aac64 => 64,
            AudioMode::Aac96 => 96,
            AudioMode::Aac128 => 128,
            AudioMode::Aac160 => 160,
            AudioMode::Off => 0,
        }
    }

    pub fn label(self) -> String {
        match self {
            AudioMode::Off => "No audio".to_string(),
            m => format!("AAC {} kbps", m.kbps()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackMode {
    Mix,
    First,
    KeepAll,
}

impl TrackMode {
    pub const ALL: [TrackMode; 3] = [TrackMode::Mix, TrackMode::First, TrackMode::KeepAll];

    pub fn label(self) -> &'static str {
        match self {
            TrackMode::Mix => "Mix all tracks into one",
            TrackMode::First => "First track only",
            TrackMode::KeepAll => "Keep all tracks separate",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SizePreset {
    Discord10,
    Nitro50,
    Nitro500,
    Custom,
}

impl SizePreset {
    pub const ALL: [SizePreset; 4] = [
        SizePreset::Discord10,
        SizePreset::Nitro50,
        SizePreset::Nitro500,
        SizePreset::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SizePreset::Discord10 => "10 MB (free)",
            SizePreset::Nitro50 => "50 MB (Nitro Basic)",
            SizePreset::Nitro500 => "500 MB (Nitro)",
            SizePreset::Custom => "Custom",
        }
    }

    /// Target in MiB. 9.5 MiB sits safely under 10 MB.
    pub fn target_mib(self, custom: f64) -> f64 {
        match self {
            SizePreset::Discord10 => 9.5,
            SizePreset::Nitro50 => 47.5,
            SizePreset::Nitro500 => 475.0,
            SizePreset::Custom => custom.max(0.5),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EncodeSettings {
    pub codec: Codec,
    pub resolution: ResolutionMode,
    pub fps: FpsMode,
    pub speed: Speed,
    pub audio: AudioMode,
    pub tracks: TrackMode,
    pub hdr_to_sdr: bool,
}

impl EncodeSettings {
    pub fn new(codec: Codec) -> Self {
        EncodeSettings {
            codec,
            resolution: ResolutionMode::Auto,
            fps: FpsMode::Auto,
            speed: Speed::Balanced,
            audio: AudioMode::Aac96,
            tracks: TrackMode::Mix,
            hdr_to_sdr: true,
        }
    }
}

// ============================================================
// PLANNING
// ============================================================

/// What the encoder will produce for a given bitrate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub fps_filter: bool,
}

/// Bits per pixel per frame below which a codec starts to visibly fall apart.
/// Auto resolution picks the largest size that stays above this.
fn bpp_target(codec: Codec) -> f64 {
    match (codec.backend, codec.family) {
        (Backend::Software, Family::H264) => 0.042,
        (Backend::Software, Family::H265) => 0.030,
        (Backend::Software, Family::Av1) => 0.025,
        (_, Family::H264) => 0.065,
        (_, Family::H265) => 0.045,
        (_, Family::Av1) => 0.035,
    }
}

fn even_floor(x: f64) -> u32 {
    let v = x.floor().max(2.0) as u32;
    (v - v % 2).max(2)
}

/// Scales so the short side equals `short`, keeping aspect ratio; never upscales.
fn scaled_dims(w: u32, h: u32, short: u32) -> (u32, u32) {
    let src_short = w.min(h);
    if short >= src_short {
        return (even_floor(w as f64), even_floor(h as f64));
    }
    let factor = short as f64 / src_short as f64;
    if w >= h {
        (even_floor((w as f64 * factor).round()), even_floor(short as f64))
    } else {
        (even_floor(short as f64), even_floor((h as f64 * factor).round()))
    }
}

/// Drops whole frames (60→30, 50→25, 144→28.8) so motion stays even.
fn decimated_fps(src: f64, cap: f64) -> f64 {
    if src <= cap + 0.5 {
        src
    } else {
        src / (src / cap).ceil()
    }
}

fn bpp(bitrate: u64, w: u32, h: u32, fps: f64) -> f64 {
    bitrate as f64 / (w as f64 * h as f64 * fps.max(1.0))
}

pub fn plan(settings: &EncodeSettings, info: &MediaInfo, bitrate: u64) -> Plan {
    let (sw, sh) = (info.width, info.height);
    let src_fps = info.fps.max(1.0);
    let src_short = sw.min(sh);
    let target = bpp_target(settings.codec);

    let mut ladder = vec![src_short];
    ladder.extend(
        [2160u32, 1440, 1080, 900, 720, 540, 480, 360, 240]
            .into_iter()
            .filter(|&s| s < src_short),
    );

    let fit = |fps: f64| -> (u32, u32) {
        for &short in &ladder {
            let (w, h) = scaled_dims(sw, sh, short);
            if bpp(bitrate, w, h, fps) >= target {
                return (w, h);
            }
        }
        scaled_dims(sw, sh, *ladder.last().unwrap_or(&src_short))
    };

    let fixed = |mode: ResolutionMode| -> (u32, u32) {
        match mode {
            ResolutionMode::Short(s) => scaled_dims(sw, sh, s),
            _ => (even_floor(sw as f64), even_floor(sh as f64)),
        }
    };

    let half = decimated_fps(src_fps, 30.0);
    let fps = match settings.fps {
        FpsMode::Original => src_fps,
        FpsMode::Max60 => decimated_fps(src_fps, 60.0),
        FpsMode::Max30 => half,
        FpsMode::Auto => {
            if half >= src_fps - 0.01 {
                src_fps
            } else {
                match settings.resolution {
                    // Benchmarks showed dropping frames costs more than it saves, so keep the
                    // full frame rate unless the picture would otherwise fall below 480p.
                    ResolutionMode::Auto => {
                        let (w, h) = fit(src_fps);
                        if w.min(h) >= 480.min(src_short) {
                            src_fps
                        } else {
                            half
                        }
                    }
                    mode => {
                        let (w, h) = fixed(mode);
                        if bpp(bitrate, w, h, src_fps) >= target {
                            src_fps
                        } else {
                            half
                        }
                    }
                }
            }
        }
    };

    let (width, height) = match settings.resolution {
        ResolutionMode::Auto => fit(fps),
        mode => fixed(mode),
    };

    Plan {
        width,
        height,
        fps,
        fps_filter: (fps - src_fps).abs() > 0.01,
    }
}

/// How many audio tracks end up in the file.
pub fn audio_tracks_out(settings: &EncodeSettings, info: &MediaInfo) -> usize {
    if info.audio_tracks == 0 || settings.audio == AudioMode::Off {
        return 0;
    }
    match settings.tracks {
        TrackMode::Mix | TrackMode::First => 1,
        TrackMode::KeepAll => info.audio_tracks,
    }
}

fn audio_bps(settings: &EncodeSettings, info: &MediaInfo) -> f64 {
    settings.audio.kbps() as f64 * 1000.0 * audio_tracks_out(settings, info) as f64
}

/// x264, x265 and libaom do a real analysis pass. SVT-AV1 and rav1e don't support FFmpeg's
/// two-pass mode (the first pass would just be wasted time), and GPU encoders are single-pass.
pub fn is_two_pass(codec: Codec, caps: &Caps) -> bool {
    match (codec.backend, codec.family) {
        (Backend::Software, Family::H264) | (Backend::Software, Family::H265) => true,
        (Backend::Software, Family::Av1) => caps.av1 == Some(Av1Encoder::Aom),
        _ => false,
    }
}

/// Single-pass encoders land less precisely, so they aim a little lower.
fn safety(two_pass: bool) -> f64 {
    if two_pass {
        0.93
    } else {
        0.87
    }
}

/// Usable bits (after the safety margin) minus audio for every kept track, spread over the clip.
fn initial_bitrate(target_bytes: u64, duration: f64, audio_bps: f64, safety: f64) -> u64 {
    let usable = target_bytes as f64 * 8.0 * safety;
    let video = (usable - audio_bps * duration) / duration;
    (video.max(0.0) as u64).max(MIN_VIDEO_BPS)
}

pub struct Estimate {
    pub plan: Plan,
    pub video_bps: u64,
    pub audio_bps: u64,
    /// The size limit leaves very little for video; the result will look rough.
    pub tight: bool,
}

pub fn estimate(
    settings: &EncodeSettings,
    info: &MediaInfo,
    duration: f64,
    target_bytes: u64,
    caps: &Caps,
) -> Estimate {
    let audio = audio_bps(settings, info);
    let two_pass = is_two_pass(settings.codec, caps);
    let video = initial_bitrate(target_bytes, duration.max(0.01), audio, safety(two_pass));
    Estimate {
        plan: plan(settings, info, video),
        video_bps: video,
        audio_bps: audio as u64,
        tight: video < 150_000,
    }
}

// ============================================================
// FFMPEG ARGUMENTS
// ============================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pass {
    Single,
    First,
    Second,
}

fn s(v: impl ToString) -> String {
    v.to_string()
}

/// Video encoder arguments. Shared with GPU detection so the self-test runs the real settings.
pub fn video_codec_args(
    codec: Codec,
    av1: Option<Av1Encoder>,
    gpu: Option<&GpuEncoder>,
    speed: Speed,
    bitrate: u64,
    pass: Pass,
) -> Result<Vec<String>, String> {
    let b = s(bitrate);
    let maxrate = s(bitrate * 3 / 2);
    let bufsize = s(bitrate * 2);
    let mut a: Vec<String> = Vec::new();

    if codec.backend == Backend::Software {
        match codec.family {
            Family::H264 => {
                let preset = match speed {
                    Speed::Faster => "medium",
                    Speed::Balanced => "slow",
                    Speed::Best => "slower",
                };
                a.extend(
                    ["-c:v", "libx264", "-preset", preset, "-profile:v", "high", "-b:v", &b]
                        .map(s),
                );
                // aq-mode 3 biases bits toward dark areas, which reduces banding in dark scenes.
                a.extend(["-x264-params", "aq-mode=3"].map(s));
                match pass {
                    Pass::First => a.extend(["-pass", "1", "-passlogfile", "passlog"].map(s)),
                    Pass::Second => a.extend(["-pass", "2", "-passlogfile", "passlog"].map(s)),
                    Pass::Single => {}
                }
            }
            Family::H265 => {
                let preset = match speed {
                    Speed::Faster => "medium",
                    Speed::Balanced => "slow",
                    Speed::Best => "slower",
                };
                // libx265 ignores FFmpeg's -pass flag, so two-pass has to go through x265-params.
                // A fast first pass costs almost nothing in quality.
                let mut params = String::from("log-level=error:aq-mode=3");
                match pass {
                    Pass::First => params.push_str(":pass=1:stats=passlog:slow-firstpass=0"),
                    Pass::Second => params.push_str(":pass=2:stats=passlog"),
                    Pass::Single => {}
                }
                a.extend(
                    ["-c:v", "libx265", "-preset", preset, "-profile:v", "main", "-b:v", &b]
                        .map(s),
                );
                a.extend(["-x265-params".to_string(), params]);
            }
            Family::Av1 => match av1 {
                Some(Av1Encoder::Svt) => {
                    let preset = match speed {
                        Speed::Faster => "8",
                        Speed::Balanced => "6",
                        Speed::Best => "4",
                    };
                    // tune=0 optimizes for how it looks rather than for PSNR.
                    a.extend(
                        ["-c:v", "libsvtav1", "-preset", preset, "-b:v", &b, "-svtav1-params", "tune=0"]
                            .map(s),
                    );
                }
                Some(Av1Encoder::Aom) => {
                    let cpu = match speed {
                        Speed::Faster => "6",
                        Speed::Balanced => "4",
                        Speed::Best => "3",
                    };
                    a.extend(
                        ["-c:v", "libaom-av1", "-cpu-used", cpu, "-row-mt", "1", "-b:v", &b].map(s),
                    );
                    match pass {
                        Pass::First => a.extend(["-pass", "1", "-passlogfile", "passlog"].map(s)),
                        Pass::Second => a.extend(["-pass", "2", "-passlogfile", "passlog"].map(s)),
                        Pass::Single => {}
                    }
                }
                Some(Av1Encoder::Rav1e) => {
                    let sp = match speed {
                        Speed::Faster => "8",
                        Speed::Balanced => "6",
                        Speed::Best => "4",
                    };
                    a.extend(["-c:v", "librav1e", "-speed", sp, "-b:v", &b].map(s));
                }
                None => return Err("FFmpeg has no AV1 encoder.".to_string()),
            },
        }
        return Ok(a);
    }

    let gpu = gpu.ok_or_else(|| format!("{} is not available on this computer.", codec.label()))?;
    let enc = gpu.encoder.as_str();
    let profile: Option<&str> = match codec.family {
        Family::H264 => Some("high"),
        Family::H265 => Some("main"),
        Family::Av1 => None,
    };

    match codec.backend {
        Backend::Nvenc => {
            let preset = match speed {
                Speed::Faster => "p5",
                Speed::Balanced => "p6",
                Speed::Best => "p7",
            };
            a.extend(
                [
                    "-c:v", enc, "-preset", preset, "-tune", "hq", "-rc", "vbr", "-multipass", "fullres",
                    "-b:v", &b, "-maxrate", &maxrate, "-bufsize", &bufsize, "-spatial-aq", "1",
                    "-temporal-aq", "1", "-rc-lookahead", "32",
                ]
                .map(s),
            );
        }
        Backend::Qsv => {
            let preset = match speed {
                Speed::Faster => "medium",
                Speed::Balanced => "slow",
                Speed::Best => "veryslow",
            };
            a.extend(
                ["-c:v", enc, "-preset", preset, "-b:v", &b, "-maxrate", &maxrate, "-bufsize", &bufsize]
                    .map(s),
            );
        }
        Backend::Vaapi => {
            a.extend(
                ["-c:v", enc, "-rc_mode", "VBR", "-b:v", &b, "-maxrate", &maxrate, "-bufsize", &bufsize]
                    .map(s),
            );
        }
        Backend::Amf => {
            a.extend(
                [
                    "-c:v", enc, "-quality", "quality", "-rc", "vbr_peak", "-b:v", &b, "-maxrate", &maxrate,
                    "-bufsize", &bufsize,
                ]
                .map(s),
            );
        }
        Backend::VideoToolbox => {
            a.extend(["-c:v", enc, "-b:v", &b].map(s));
        }
        Backend::Software => unreachable!(),
    }
    if let Some(p) = profile {
        a.extend(["-profile:v", p].map(s));
    }
    Ok(a)
}

/// Arguments that must come before `-i` for some GPU encoders.
pub fn gpu_pre_input_args(gpu: &GpuEncoder) -> Vec<String> {
    match (gpu.codec.backend, &gpu.device) {
        (Backend::Vaapi, Some(dev)) => vec!["-vaapi_device".to_string(), dev.clone()],
        _ => Vec::new(),
    }
}

/// VA-API encoders need frames uploaded to the GPU at the end of the filter chain.
pub fn gpu_filter_tail(gpu: &GpuEncoder) -> Option<&'static str> {
    (gpu.codec.backend == Backend::Vaapi).then_some("hwupload")
}

/// 8-bit 4:2:0 for H.264/H.265 so every player handles it (10-bit or 4:4:4 sources would
/// otherwise produce files Discord can't play). 10-bit for AV1, which reduces banding at
/// no compatibility cost because 10-bit is part of AV1's main profile.
fn pixel_format(codec: Codec) -> &'static str {
    match (codec.backend, codec.family) {
        (Backend::Software, Family::Av1) => "yuv420p10le",
        (Backend::Software, _) => "yuv420p",
        _ => "nv12",
    }
}

fn tonemap_chain(hdr: &HdrInfo) -> String {
    format!(
        "zscale=tin={}:pin={}:min={}:t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,\
         tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv",
        hdr.transfer, hdr.primaries, hdr.matrix
    )
}

pub struct JobSpec {
    pub item_id: u64,
    pub input: PathBuf,
    pub output_dir: PathBuf,
    pub output_name: String,
    pub start: f64,
    pub end: f64,
    pub settings: EncodeSettings,
    pub info: MediaInfo,
    pub target_bytes: u64,
    pub target_mib: f64,
}

impl JobSpec {
    fn tonemap(&self, caps: &Caps) -> Option<&HdrInfo> {
        if self.settings.hdr_to_sdr && caps.tonemap {
            self.info.hdr.as_ref()
        } else {
            None
        }
    }
}

fn video_filter_chain(spec: &JobSpec, caps: &Caps, plan: &Plan, gpu: Option<&GpuEncoder>) -> String {
    let mut f: Vec<String> = Vec::new();
    if plan.fps_filter {
        f.push(format!("fps={:.6}", plan.fps));
    }
    if (plan.width, plan.height) != (spec.info.width, spec.info.height) {
        f.push(format!("scale={}:{}:flags=lanczos", plan.width, plan.height));
    }
    if let Some(hdr) = spec.tonemap(caps) {
        f.push(tonemap_chain(hdr));
    }
    f.push(format!("format={}", pixel_format(spec.settings.codec)));
    if let Some(tail) = gpu.and_then(gpu_filter_tail) {
        f.push(tail.to_string());
    }
    f.join(",")
}

fn build_args(
    spec: &JobSpec,
    caps: &Caps,
    plan: &Plan,
    bitrate: u64,
    pass: Pass,
    hwaccel: Option<&str>,
    output: &Path,
) -> Result<Vec<OsString>, String> {
    let codec = spec.settings.codec;
    let gpu = if codec.is_gpu() {
        Some(
            caps.gpu_for(codec)
                .ok_or_else(|| format!("{} is not available on this computer.", codec.label()))?,
        )
    } else {
        None
    };
    let duration = spec.end - spec.start;
    let vi = spec.info.video_index;
    let vf = video_filter_chain(spec, caps, plan, gpu);
    let tracks_out = audio_tracks_out(&spec.settings, &spec.info);

    let mut a: Vec<OsString> = Vec::new();
    let mut push = |v: &str| a.push(OsString::from(v));
    for v in ["-hide_banner", "-nostdin", "-y", "-v", "error", "-nostats", "-progress", "pipe:1"] {
        push(v);
    }
    if let Some(g) = gpu {
        for v in gpu_pre_input_args(g) {
            push(&v);
        }
    }
    if let Some(h) = hwaccel {
        push("-hwaccel");
        push(h);
    }
    push("-ss");
    push(&format!("{:.3}", spec.start));
    push("-i");
    a.push(spec.input.clone().into_os_string());
    let mut push = |v: &str| a.push(OsString::from(v));
    push("-t");
    push(&format!("{:.3}", duration));

    let mix = pass != Pass::First
        && tracks_out > 0
        && spec.settings.tracks == TrackMode::Mix
        && spec.info.audio_tracks > 1;

    if mix {
        let inputs: String = (0..spec.info.audio_tracks).map(|i| format!("[0:a:{i}]")).collect();
        push("-filter_complex");
        push(&format!(
            "[0:{vi}]{vf}[v];{inputs}amix=inputs={}:duration=longest:normalize=0[a]",
            spec.info.audio_tracks
        ));
        push("-map");
        push("[v]");
        push("-map");
        push("[a]");
    } else {
        push("-map");
        push(&format!("0:{vi}"));
        push("-vf");
        push(&vf);
        if pass == Pass::First || tracks_out == 0 {
            push("-an");
        } else if spec.settings.tracks == TrackMode::KeepAll {
            push("-map");
            push("0:a");
        } else {
            push("-map");
            push("0:a:0");
        }
    }

    for v in video_codec_args(codec, caps.av1, gpu, spec.settings.speed, bitrate, pass)? {
        push(&v);
    }

    if pass != Pass::First && tracks_out > 0 {
        for v in ["-c:a", caps.aac.as_str(), "-b:a"] {
            push(v);
        }
        push(&format!("{}k", spec.settings.audio.kbps()));
        // Downmix surround to stereo: Discord plays stereo, and 96 kbps spread over
        // six channels sounds bad.
        push("-ac");
        push("2");
    }

    if codec.family == Family::H265 {
        // Apple devices (and so Discord on iOS) need the hvc1 tag to play HEVC in MP4.
        push("-tag:v");
        push("hvc1");
    }
    if spec.tonemap(caps).is_some() {
        for v in ["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709"] {
            push(v);
        }
    }

    if pass == Pass::First {
        for v in ["-f", "null", NULL_DEVICE] {
            push(v);
        }
    } else {
        for v in ["-movflags", "+faststart", "-f", "mp4"] {
            push(v);
        }
        a.push(output.as_os_str().to_owned());
    }
    Ok(a)
}

// ============================================================
// RUNNING FFMPEG
// ============================================================

#[derive(Debug)]
enum RunError {
    Cancelled,
    Failed(String),
}

/// Lets the UI stop the job: sets a flag and kills whichever FFmpeg is running.
#[derive(Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl CancelToken {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.child.lock() {
            if let Some(child) = guard.as_mut() {
                let _ = child.kill();
            }
        }
    }

    fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    fn set_child(&self, child: Child) {
        let mut guard = self.child.lock().unwrap_or_else(|p| p.into_inner());
        *guard = Some(child);
        if self.is_cancelled() {
            if let Some(c) = guard.as_mut() {
                let _ = c.kill();
            }
        }
    }

    fn wait_child(&self) -> Option<std::io::Result<ExitStatus>> {
        let child = self.child.lock().unwrap_or_else(|p| p.into_inner()).take();
        child.map(|mut c| c.wait())
    }
}

fn run_ffmpeg(
    tools: &Tools,
    args: &[OsString],
    cwd: &Path,
    cancel: &CancelToken,
    duration: f64,
    on_progress: &mut dyn FnMut(f64, Option<f64>),
) -> Result<(), RunError> {
    if cancel.is_cancelled() {
        return Err(RunError::Cancelled);
    }
    let mut child = ffmpeg::command(&tools.ffmpeg)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| RunError::Failed(format!("Could not start FFmpeg: {e}")))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let err_reader = thread::spawn(move || {
        let mut lines: VecDeque<String> = VecDeque::new();
        if let Some(stderr) = stderr {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let line = line.trim().to_string();
                if !line.is_empty() {
                    lines.push_back(line);
                    if lines.len() > 40 {
                        lines.pop_front();
                    }
                }
            }
        }
        lines
    });
    cancel.set_child(child);

    let mut speed: Option<f64> = None;
    if let Some(stdout) = stdout {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let line = line.trim();
            // out_time_ms is (despite the name) also microseconds.
            if let Some(v) = line
                .strip_prefix("out_time_us=")
                .or_else(|| line.strip_prefix("out_time_ms="))
            {
                if let Ok(us) = v.parse::<i64>() {
                    if duration > 0.0 {
                        on_progress((us as f64 / 1_000_000.0 / duration).clamp(0.0, 1.0), speed);
                    }
                }
            } else if let Some(v) = line.strip_prefix("speed=") {
                speed = v.trim().trim_end_matches('x').trim().parse::<f64>().ok();
            }
        }
    }

    let status = cancel.wait_child();
    let lines = err_reader.join().unwrap_or_default();
    if cancel.is_cancelled() {
        return Err(RunError::Cancelled);
    }
    match status {
        Some(Ok(st)) if st.success() => Ok(()),
        Some(Ok(st)) => {
            let details: Vec<String> = lines.iter().rev().take(10).rev().cloned().collect();
            if details.is_empty() {
                Err(RunError::Failed(match st.code() {
                    Some(code) => format!("FFmpeg exited with code {code}."),
                    None => "FFmpeg was stopped unexpectedly.".to_string(),
                }))
            } else {
                Err(RunError::Failed(details.join("\n")))
            }
        }
        _ => Err(RunError::Failed("FFmpeg was stopped unexpectedly.".to_string())),
    }
}

// ============================================================
// WORKER
// ============================================================

pub enum WorkerMsg {
    Progress {
        fraction: f32,
        speed: Option<f64>,
        eta_secs: Option<f64>,
    },
    Status(String),
    Done {
        path: PathBuf,
        size: u64,
    },
    Failed(String),
    Cancelled,
}

pub struct Worker {
    pub item_id: u64,
    pub rx: Receiver<WorkerMsg>,
    pub cancel: CancelToken,
}

/// Holds pass logs; removed when the job ends however it ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> std::io::Result<TempDir> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("clipcram-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(TempDir(dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Partial outputs written next to the final file; deleted unless one is kept.
struct Partials(Vec<PathBuf>);

impl Drop for Partials {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

pub fn spawn(tools: Tools, caps: Arc<Caps>, spec: JobSpec, ctx: eframe::egui::Context) -> Worker {
    let (tx, rx) = mpsc::channel();
    let cancel = CancelToken::default();
    let item_id = spec.item_id;
    let token = cancel.clone();
    thread::spawn(move || {
        let ctx2 = ctx.clone();
        let send = |msg: WorkerMsg| {
            let _ = tx.send(msg);
            ctx2.request_repaint();
        };
        let result = run_job(&tools, &caps, &spec, &token, &send);
        match result {
            Ok((path, size)) => send(WorkerMsg::Done { path, size }),
            Err(RunError::Cancelled) => send(WorkerMsg::Cancelled),
            Err(RunError::Failed(e)) => send(WorkerMsg::Failed(e)),
        }
    });
    Worker { item_id, rx, cancel }
}

struct ProgressReporter<'a> {
    send: &'a dyn Fn(WorkerMsg),
    started: Instant,
    last_sent: Option<Instant>,
}

impl ProgressReporter<'_> {
    fn report(&mut self, fraction: f64, speed: Option<f64>) {
        let now = Instant::now();
        if self
            .last_sent
            .is_some_and(|t| now.duration_since(t).as_millis() < 120 && fraction < 0.999)
        {
            return;
        }
        self.last_sent = Some(now);
        let elapsed = self.started.elapsed().as_secs_f64();
        let eta = (fraction > 0.02).then(|| elapsed * (1.0 - fraction) / fraction);
        (self.send)(WorkerMsg::Progress {
            fraction: fraction as f32,
            speed,
            eta_secs: eta,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn run_pass(
    tools: &Tools,
    caps: &Caps,
    spec: &JobSpec,
    plan: &Plan,
    bitrate: u64,
    pass: Pass,
    hwaccel: &mut Option<String>,
    output: &Path,
    tmp: &Path,
    cancel: &CancelToken,
    range: (f64, f64),
    progress: &mut ProgressReporter,
    send: &dyn Fn(WorkerMsg),
) -> Result<(), RunError> {
    let duration = spec.end - spec.start;
    loop {
        let args = build_args(spec, caps, plan, bitrate, pass, hwaccel.as_deref(), output)
            .map_err(RunError::Failed)?;
        let mut on_progress = |f: f64, speed: Option<f64>| {
            progress.report(range.0 + (range.1 - range.0) * f, speed);
        };
        match run_ffmpeg(tools, &args, tmp, cancel, duration, &mut on_progress) {
            Err(RunError::Failed(_)) if hwaccel.is_some() => {
                // Some files trip up hardware decoders; the CPU decoder always works.
                send(WorkerMsg::Status(
                    "Hardware decoding failed for this file, retrying with CPU decoding".to_string(),
                ));
                *hwaccel = None;
            }
            other => return other,
        }
    }
}

fn run_job(
    tools: &Tools,
    caps: &Caps,
    spec: &JobSpec,
    cancel: &CancelToken,
    send: &dyn Fn(WorkerMsg),
) -> Result<(PathBuf, u64), RunError> {
    let duration = spec.end - spec.start;
    if duration <= 0.0 {
        return Err(RunError::Failed("End time must be greater than start time.".to_string()));
    }
    std::fs::create_dir_all(&spec.output_dir).map_err(|e| {
        RunError::Failed(format!("Could not create:\n\n{}\n\n{e}", spec.output_dir.display()))
    })?;
    let tmp = TempDir::new().map_err(|e| RunError::Failed(format!("Could not create a temporary folder: {e}")))?;

    let codec = spec.settings.codec;
    let two_pass = is_two_pass(codec, caps);
    let target = spec.target_bytes;
    let mut bitrate = initial_bitrate(target, duration, audio_bps(&spec.settings, &spec.info), safety(two_pass));
    let mut hwaccel = caps.hwaccel.clone();
    let mut stats_for: Option<Plan> = None;
    let mut locked_plan: Option<Plan> = None;
    let mut fill_tried = false;
    let mut best: Option<(PathBuf, u64)> = None;
    let mut partials = Partials(Vec::new());
    let name = util::sanitize_filename(&spec.output_name);
    let mut progress = ProgressReporter {
        send,
        started: Instant::now(),
        last_sent: None,
    };

    for attempt in 0..MAX_ATTEMPTS {
        let plan = locked_plan.unwrap_or_else(|| plan(&spec.settings, &spec.info, bitrate));
        progress.started = Instant::now();
        send(WorkerMsg::Status(format!(
            "{} • {}×{} • {} fps • {} kbps",
            codec.label(),
            plan.width,
            plan.height,
            format_fps(plan.fps),
            util::thousands(bitrate / 1000)
        )));

        let partial = spec.output_dir.join(format!(".{name}.part{attempt}"));
        partials.0.push(partial.clone());
        let _ = std::fs::remove_file(&partial);

        if two_pass {
            let need_first = stats_for != Some(plan);
            let second_range = if need_first {
                send(WorkerMsg::Status(format!("Pass 1/2 (attempt {}/{MAX_ATTEMPTS})", attempt + 1)));
                run_pass(
                    tools, caps, spec, &plan, bitrate, Pass::First, &mut hwaccel, &partial,
                    &tmp.0, cancel, (0.0, 0.35), &mut progress, send,
                )?;
                stats_for = Some(plan);
                (0.35, 0.97)
            } else {
                (0.0, 0.97)
            };
            send(WorkerMsg::Status(if need_first {
                format!("Pass 2/2 (attempt {}/{MAX_ATTEMPTS})", attempt + 1)
            } else {
                format!(
                    "Pass 2/2 (attempt {}/{MAX_ATTEMPTS}, reusing the first-pass analysis)",
                    attempt + 1
                )
            }));
            run_pass(
                tools, caps, spec, &plan, bitrate, Pass::Second, &mut hwaccel, &partial,
                &tmp.0, cancel, second_range, &mut progress, send,
            )?;
        } else {
            send(WorkerMsg::Status(format!("Encoding (attempt {}/{MAX_ATTEMPTS})", attempt + 1)));
            run_pass(
                tools, caps, spec, &plan, bitrate, Pass::Single, &mut hwaccel, &partial,
                &tmp.0, cancel, (0.0, 0.97), &mut progress, send,
            )?;
        }

        let size = std::fs::metadata(&partial)
            .map(|m| m.len())
            .map_err(|_| RunError::Failed("FFmpeg completed without creating the output file.".to_string()))?;
        send(WorkerMsg::Status(format!("Output: {:.2} MiB", util::mib(size))));

        if size <= target {
            if let Some((old, _)) = best.replace((partial.clone(), size)) {
                let _ = std::fs::remove_file(old);
            }
            // With "Best quality", use the space that's left: re-run only the second pass
            // (the first-pass analysis is reused) at a higher bitrate.
            let room = size < (target as f64 * 0.90) as u64;
            if spec.settings.speed == Speed::Best
                && two_pass
                && room
                && !fill_tried
                && attempt + 1 < MAX_ATTEMPTS
            {
                fill_tried = true;
                locked_plan = Some(plan);
                bitrate = (bitrate as f64 * target as f64 / size.max(1) as f64 * 0.97) as u64;
                send(WorkerMsg::Status(format!(
                    "{:.2} MiB leaves room under the limit, encoding again at {} kbps for extra quality",
                    util::mib(size),
                    util::thousands(bitrate / 1000)
                )));
                continue;
            }
            break;
        }

        let _ = std::fs::remove_file(&partial);
        if best.is_some() {
            // The quality top-up overshot; keep the earlier result that fit.
            break;
        }
        locked_plan = None;
        let ratio = target as f64 / size as f64;
        bitrate = ((bitrate as f64 * ratio * 0.96) as u64).max(MIN_VIDEO_BPS);
    }

    let Some((partial, size)) = best else {
        return Err(RunError::Failed(format!(
            "Could not get the output below {:.1} MiB after five encoding attempts.",
            spec.target_mib
        )));
    };

    let final_path = util::unique_output_path(&spec.output_dir, &name);
    std::fs::rename(&partial, &final_path)
        .or_else(|_| std::fs::copy(&partial, &final_path).map(|_| ()))
        .map_err(|e| RunError::Failed(format!("Could not save {}: {e}", final_path.display())))?;
    send(WorkerMsg::Progress {
        fraction: 1.0,
        speed: None,
        eta_secs: Some(0.0),
    });
    Ok((final_path, size))
}

pub fn format_fps(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.01 {
        format!("{}", fps.round() as i64)
    } else {
        format!("{fps:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(w: u32, h: u32, fps: f64) -> MediaInfo {
        MediaInfo {
            duration: 60.0,
            width: w,
            height: h,
            fps,
            video_index: 0,
            audio_tracks: 1,
            hdr: None,
            video_codec: "h264".into(),
        }
    }

    #[test]
    fn dims_and_fps() {
        assert_eq!(scaled_dims(1920, 1080, 720), (1280, 720));
        assert_eq!(scaled_dims(1080, 1920, 720), (720, 1280));
        assert_eq!(scaled_dims(1919, 1079, 2160), (1918, 1078));
        assert_eq!(decimated_fps(60.0, 30.0), 30.0);
        assert_eq!(decimated_fps(50.0, 30.0), 25.0);
        assert_eq!(decimated_fps(30.0, 30.0), 30.0);
        assert!((decimated_fps(59.94, 30.0) - 29.97).abs() < 0.001);
    }

    #[test]
    fn auto_plan_examples() {
        let s = EncodeSettings::new(Codec::H265);
        // 30 s of 1080p60 into 9.5 MiB with HEVC keeps 60 fps at 720p.
        let target = (9.5 * 1024.0 * 1024.0) as u64;
        let b = initial_bitrate(target, 30.0, 96_000.0, 0.93);
        let p = plan(&s, &info(1920, 1080, 60.0), b);
        assert_eq!((p.width, p.height), (1280, 720));
        assert!(!p.fps_filter);
        // 60 s keeps 60 fps at 540p.
        let b = initial_bitrate(target, 60.0, 96_000.0, 0.93);
        let p = plan(&s, &info(1920, 1080, 60.0), b);
        assert_eq!((p.width, p.height, p.fps as u32), (960, 540, 60));
        // 4 minutes would fall below 480p at 60 fps, so it halves the frame rate.
        let b = initial_bitrate(target, 240.0, 96_000.0, 0.93);
        let p = plan(&s, &info(1920, 1080, 60.0), b);
        assert_eq!(p.fps as u32, 30);
        assert!(p.fps_filter);
        // Original resolution keeps the source size (rounded to even numbers).
        let mut s2 = s;
        s2.resolution = ResolutionMode::Original;
        s2.fps = FpsMode::Original;
        let p = plan(&s2, &info(1921, 1081, 60.0), b);
        assert_eq!((p.width, p.height), (1920, 1080));
    }

    /// End-to-end check against a real file: `DC_TEST_INPUT=clip.mkv cargo test --release -- --ignored --nocapture`.
    /// Optional: DC_TEST_CODECS=h265,h264,av1  DC_TEST_MIB=1.5  DC_TEST_SPEED=faster|balanced|best
    #[test]
    #[ignore]
    fn real_encodes() {
        let input = PathBuf::from(std::env::var("DC_TEST_INPUT").expect("set DC_TEST_INPUT"));
        let tools = Tools::find().expect("ffmpeg and ffprobe on PATH");
        let caps = Caps::detect(&tools);
        let info = ffmpeg::probe(&tools, &input).expect("probe");
        println!("source: {}x{} {:.2} fps {:.2}s, {} audio tracks, hdr={:?}", info.width, info.height, info.fps, info.duration, info.audio_tracks, info.hdr);
        let target_mib: f64 = std::env::var("DC_TEST_MIB").ok().and_then(|v| v.parse().ok()).unwrap_or(1.5);
        let speed = match std::env::var("DC_TEST_SPEED").as_deref() {
            Ok("best") => Speed::Best,
            Ok("balanced") => Speed::Balanced,
            _ => Speed::Faster,
        };
        let codecs = std::env::var("DC_TEST_CODECS").unwrap_or_else(|_| "h265,h264,av1".into());
        let out_dir = std::env::temp_dir().join("dc-test-out");
        let _ = std::fs::create_dir_all(&out_dir);
        for name in codecs.split(',') {
            let codec = match name.trim() {
                "h265" => Codec::H265,
                "h264" => Codec::H264,
                _ => Codec::AV1,
            };
            let mut settings = EncodeSettings::new(codec);
            settings.speed = speed;
            if std::env::var("DC_TEST_RES").as_deref() == Ok("original") {
                settings.resolution = ResolutionMode::Original;
            }
            if std::env::var("DC_TEST_FPS").as_deref() == Ok("original") {
                settings.fps = FpsMode::Original;
            }
            let spec = JobSpec {
                item_id: 0,
                input: input.clone(),
                output_dir: out_dir.clone(),
                output_name: encode_test_name(&input, codec),
                start: 1.0,
                end: (info.duration - 0.5).max(1.5),
                settings,
                info: info.clone(),
                target_bytes: (target_mib * 1024.0 * 1024.0) as u64,
                target_mib,
            };
            let started = Instant::now();
            let attempts = std::cell::Cell::new(0u32);
            let send = |m: WorkerMsg| {
                if let WorkerMsg::Status(s) = m {
                    if s.starts_with("Output:") {
                        attempts.set(attempts.get() + 1);
                    }
                    println!("    {s}");
                }
            };
            println!("== {}", codec.label());
            let result = run_job(&tools, &caps, &spec, &CancelToken::default(), &send);
            match result {
                Ok((path, size)) => {
                    let out = ffmpeg::probe(&tools, &path).expect("probe output");
                    println!(
                        "  OK {:.3} MiB of {target_mib} MiB ({:.0}%) -> {}x{} {:.2} fps {:.2}s, {} audio, {:.1}s wall",
                        util::mib(size),
                        util::mib(size) / target_mib * 100.0,
                        out.width,
                        out.height,
                        out.fps,
                        out.duration,
                        out.audio_tracks,
                        started.elapsed().as_secs_f64()
                    );
                    println!(
                        "RESULT|{}|{:.1}|{}|{}|{}x{}|{:.2}|{}",
                        codec.short(),
                        started.elapsed().as_secs_f64(),
                        size,
                        path.display(),
                        out.width,
                        out.height,
                        out.fps,
                        attempts.get()
                    );
                    assert!(size <= spec.target_bytes);
                    assert!((out.duration - (spec.end - spec.start)).abs() < 0.25, "duration mismatch");
                }
                Err(RunError::Failed(e)) => panic!("{} failed: {e}", codec.label()),
                Err(RunError::Cancelled) => panic!("cancelled?"),
            }
        }
    }

    fn encode_test_name(input: &Path, codec: Codec) -> String {
        default_output_name(input, codec)
    }

    #[test]
    fn x265_two_pass_goes_through_x265_params() {
        let a = video_codec_args(Codec::H265, None, None, Speed::Balanced, 1_000_000, Pass::First).unwrap();
        let joined = a.join(" ");
        assert!(joined.contains("pass=1:stats=passlog:slow-firstpass=0"));
        assert!(!joined.contains("-pass "));
        let a = video_codec_args(Codec::AV1, Some(Av1Encoder::Svt), None, Speed::Balanced, 1_000_000, Pass::Single).unwrap();
        assert!(!a.join(" ").contains("-pass"));
    }

    #[test]
    fn bitrate_formula() {
        let target = (9.5 * 1024.0 * 1024.0) as u64;
        let b = initial_bitrate(target, 30.0, 96_000.0, 0.93);
        let expected = ((target as f64 * 8.0 * 0.93) - 96_000.0 * 30.0) / 30.0;
        assert_eq!(b, expected as u64);
        assert_eq!(initial_bitrate(target, 100_000.0, 96_000.0, 0.93), MIN_VIDEO_BPS);
    }
}
