//! Everything about turning one queue item into a file under the size limit:
//! settings, resolution/frame-rate planning, FFmpeg arguments, and the worker thread.

use crate::edit::{self, AudioLayout, AudioPlan, Edit, GraphSpec};
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
    /// The only vendor whose GPUs this backend drives. VA-API works with several, so its
    /// vendor comes from the device instead.
    pub fn native_vendor(self) -> Vendor {
        match self {
            Backend::Software => Vendor::None,
            Backend::Nvenc => Vendor::Nvidia,
            Backend::Amf => Vendor::Amd,
            Backend::Qsv => Vendor::Intel,
            Backend::VideoToolbox => Vendor::Apple,
            Backend::Vaapi => Vendor::Other,
        }
    }

    pub fn api_name(self) -> &'static str {
        match self {
            Backend::Software => "CPU",
            Backend::Nvenc => "NVENC",
            Backend::Amf => "AMF",
            Backend::Qsv => "Quick Sync",
            Backend::Vaapi => "VA-API",
            Backend::VideoToolbox => "VideoToolbox",
        }
    }
}

/// Who made the GPU a codec runs on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Vendor {
    /// Not a GPU (CPU encoders).
    #[default]
    None,
    Nvidia,
    Amd,
    Intel,
    Apple,
    Other,
}

impl Vendor {
    /// From a PCI vendor ID, as Linux reports it in sysfs.
    pub fn from_pci_id(id: u16) -> Vendor {
        match id {
            0x10de => Vendor::Nvidia,
            0x1002 | 0x1022 => Vendor::Amd,
            0x8086 => Vendor::Intel,
            _ => Vendor::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Vendor::None => "CPU",
            Vendor::Nvidia => "NVIDIA",
            Vendor::Amd => "AMD",
            Vendor::Intel => "Intel",
            Vendor::Apple => "Apple",
            Vendor::Other => "Other",
        }
    }
}

/// Where an encode runs: the CPU, or one particular GPU through one particular API.
/// Two GPUs from the same vendor (say an AMD integrated and an AMD discrete card) are told
/// apart by `slot`, the number of their Linux render node (renderD128 is slot 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Engine {
    pub backend: Backend,
    pub vendor: Vendor,
    pub slot: u8,
}

impl Engine {
    pub const CPU: Engine = Engine { backend: Backend::Software, vendor: Vendor::None, slot: 0 };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Codec {
    pub family: Family,
    pub backend: Backend,
    /// Missing from settings saved by older versions; `normalized` fills it in.
    #[serde(default)]
    pub vendor: Vendor,
    #[serde(default)]
    pub slot: u8,
}

impl Codec {
    pub const H265: Codec = Codec::cpu(Family::H265);
    pub const H264: Codec = Codec::cpu(Family::H264);
    pub const AV1: Codec = Codec::cpu(Family::Av1);

    pub const fn cpu(family: Family) -> Codec {
        Codec { family, backend: Backend::Software, vendor: Vendor::None, slot: 0 }
    }

    pub const fn on(engine: Engine, family: Family) -> Codec {
        Codec { family, backend: engine.backend, vendor: engine.vendor, slot: engine.slot }
    }

    pub fn engine(self) -> Engine {
        Engine { backend: self.backend, vendor: self.vendor, slot: self.slot }
    }

    /// Fills in the vendor for backends that only have one, so codecs saved by versions that
    /// didn't record it still match the ones detected now.
    pub fn normalized(self) -> Codec {
        match self.backend {
            Backend::Vaapi => self,
            b => Codec { vendor: b.native_vendor(), slot: 0, ..self },
        }
    }

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
            Backend::Vaapi => format!("{} on {} GPU via VA-API", self.family_label(), self.vendor.label()),
            b => format!("{} on {} GPU", self.family_label(), b.native_vendor().label()),
        }
    }

    pub fn short(self) -> String {
        let name = match self.family {
            Family::H265 => "HEVC",
            Family::H264 => "H264",
            Family::Av1 => "AV1",
        };
        if self.is_gpu() {
            let vendor = if self.backend == Backend::Vaapi { self.vendor } else { self.backend.native_vendor() };
            format!("{name} {}", vendor.label())
        } else {
            name.to_string()
        }
    }

    pub fn file_suffix(self) -> &'static str {
        match self.family {
            Family::H265 => "_discord_h265.mp4",
            Family::H264 => "_discord_h264.mp4",
            Family::Av1 => "_discord_av1.mp4",
        }
    }
}

pub fn default_output_name(input: &Path, codec: Codec) -> String {
    format!("{}{}", file_stem(input), codec.file_suffix())
}

fn file_stem(input: &Path) -> String {
    input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "video".to_string())
}

/// The automatic file name for these settings: `clip_discord_h265.mp4`, `clip_discord.gif`.
pub fn output_name_for(input: &Path, settings: &EncodeSettings) -> String {
    match settings.format {
        OutputFormat::Mp4 => default_output_name(input, settings.codec),
        f => format!("{}_discord.{}", file_stem(input), f.ext()),
    }
}

/// Swaps the extension of a file name for the one the format needs.
pub fn with_format_extension(name: &str, format: OutputFormat) -> String {
    let path = Path::new(name);
    let has_known = path
        .extension()
        .map(|e| ["mp4", "gif", "webp"].contains(&e.to_string_lossy().to_lowercase().as_str()))
        .unwrap_or(false);
    let stem = if has_known { path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default() } else { name.to_string() };
    format!("{stem}.{}", format.ext())
}

/// What kind of file comes out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputFormat {
    #[default]
    Mp4,
    /// Animated GIF: plays everywhere, no sound, large per second of video.
    Gif,
    /// Animated WebP: much smaller than GIF, no sound.
    Webp,
}

impl OutputFormat {
    pub const ALL: [OutputFormat; 3] = [OutputFormat::Mp4, OutputFormat::Gif, OutputFormat::Webp];

    pub fn label(self) -> &'static str {
        match self {
            OutputFormat::Mp4 => "MP4 video",
            OutputFormat::Gif => "GIF (no sound)",
            OutputFormat::Webp => "Animated WebP (no sound)",
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            OutputFormat::Mp4 => "mp4",
            OutputFormat::Gif => "gif",
            OutputFormat::Webp => "webp",
        }
    }
}

/// Light noise removal before encoding. Grain and sensor noise eat bitrate; removing
/// some of it leaves more for the picture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Denoise {
    #[default]
    Off,
    Light,
    Strong,
}

impl Denoise {
    pub const ALL: [Denoise; 3] = [Denoise::Off, Denoise::Light, Denoise::Strong];

    pub fn label(self) -> &'static str {
        match self {
            Denoise::Off => "Off",
            Denoise::Light => "Light",
            Denoise::Strong => "Strong",
        }
    }

    fn filter(self) -> Option<&'static str> {
        match self {
            Denoise::Off => None,
            Denoise::Light => Some("hqdn3d=2:1.5:3:2.25"),
            Denoise::Strong => Some("hqdn3d=4:3:6:4.5"),
        }
    }
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
    Email,
    Custom,
}

impl SizePreset {
    pub const ALL: [SizePreset; 5] = [
        SizePreset::Discord10,
        SizePreset::Nitro50,
        SizePreset::Nitro500,
        SizePreset::Email,
        SizePreset::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SizePreset::Discord10 => "10 MB (free)",
            SizePreset::Nitro50 => "50 MB (Nitro Basic)",
            SizePreset::Nitro500 => "500 MB (Nitro)",
            SizePreset::Email => "Email (25 MB attachment limit)",
            SizePreset::Custom => "Custom",
        }
    }

    /// Target in MiB. 9.5 MiB sits safely under 10 MB.
    pub fn target_mib(self, custom: f64) -> f64 {
        match self {
            SizePreset::Discord10 => 9.5,
            SizePreset::Nitro50 => 47.5,
            SizePreset::Nitro500 => 475.0,
            // Mail clients send attachments base64-encoded, which adds about a third, so a
            // 25 MB limit fits a file of about 18 MB.
            SizePreset::Email => 17.0,
            SizePreset::Custom => custom.max(0.5),
        }
    }
}

/// Saved between runs as the settings new videos start with. `serde(default)` fills in any
/// field missing from an older settings file, so adding a field never resets the rest.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EncodeSettings {
    pub codec: Codec,
    pub resolution: ResolutionMode,
    pub fps: FpsMode,
    pub speed: Speed,
    pub audio: AudioMode,
    pub tracks: TrackMode,
    pub hdr_to_sdr: bool,
    pub format: OutputFormat,
    pub denoise: Denoise,
    /// Even out loudness to -16 LUFS, the usual level for online video.
    pub normalize: bool,
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
            format: OutputFormat::Mp4,
            denoise: Denoise::Off,
            normalize: false,
        }
    }
}

impl Default for EncodeSettings {
    fn default() -> Self {
        EncodeSettings::new(Codec::H265)
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
        FpsMode::Auto if src_fps > 60.5 => decimated_fps(src_fps, 60.0),
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
    // Single-pass GPU encoders can only spend bits where the cap allows. A tight cap (it
    // was 1.5x) starves fast action, which is most gaming footage; the size check and
    // retry after each encode catch the rare overshoot instead.
    let maxrate = s(bitrate * 3);
    let bufsize = s(bitrate * 3);
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
            if gpu.tuned {
                // B-frames used as references: a clear gain at low bitrates. Needs a Turing
                // (RTX 20 / GTX 16) or newer GPU; older ones fall back to the plain settings.
                a.extend(["-bf", "3", "-b_ref_mode", "middle"].map(s));
            }
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
            if gpu.tuned {
                // Extended bitrate control with lookahead spreads bits by content, and adaptive
                // I/B placement puts keyframes on scene cuts. Some GPUs and low-power modes
                // lack these; they fall back to the plain settings.
                a.extend(
                    ["-extbrc", "1", "-look_ahead_depth", "40", "-adaptive_i", "1", "-adaptive_b", "1"].map(s),
                );
            }
        }
        Backend::Vaapi => {
            a.extend(
                ["-c:v", enc, "-rc_mode", "VBR", "-b:v", &b, "-maxrate", &maxrate, "-bufsize", &bufsize]
                    .map(s),
            );
            if gpu.tuned {
                // Without this FFmpeg sends no quality level and the driver uses its default.
                // Level 1 is the best-quality setting: on Intel (iHD) the levels run from
                // 1 (best) to 7 (fastest), and on AMD (Mesa radeonsi) level 1 selects the
                // quality preset with pre-encode analysis and adaptive quantization. Drivers
                // without quality levels just ignore it.
                a.extend(["-compression_level", "1"].map(s));
            }
        }
        Backend::Amf if gpu.tuned => {
            let preset = match speed {
                Speed::Faster => "balanced",
                Speed::Balanced => "quality",
                Speed::Best => "high_quality",
            };
            // hqvbr is AMF's high-quality VBR. Pre-analysis looks ahead to spot scene changes
            // and spread bits by content, and the motion boost helps in fast scenes.
            a.extend(
                [
                    "-c:v", enc, "-quality", preset, "-rc", "hqvbr", "-b:v", &b, "-maxrate", &maxrate,
                    "-bufsize", &bufsize, "-preanalysis", "1", "-high_motion_quality_boost_enable", "1",
                ]
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
            if gpu.tuned {
                // Don't trade quality for speed (macOS 13+), and adaptive quantization where
                // the OS supports it. Older macOS versions fall back to the plain settings.
                a.extend(["-prio_speed", "0", "-spatial_aq", "1"].map(s));
            }
        }
        Backend::Software => unreachable!(),
    }
    if let Some(p) = profile {
        a.extend(["-profile:v", p].map(s));
    }
    Ok(a)
}

/// Whether to re-encode a result that fit with lots of room to spare, at a higher bitrate.
/// Two-pass encodes only do it with Best quality, since a second-pass re-run costs real
/// time. GPU encodes are fast and aim low (they land less precisely), so they do it unless
/// the user chose Faster.
fn fills_leftover_space(codec: Codec, speed: Speed, two_pass: bool) -> bool {
    if two_pass {
        speed == Speed::Best
    } else {
        codec.is_gpu() && speed != Speed::Faster
    }
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

pub(crate) fn tonemap_chain(hdr: &HdrInfo) -> String {
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
    pub edit: Edit,
    /// Copy the streams without re-encoding when the clip already fits and plays on Discord.
    pub copy_if_fits: bool,
    /// Compare the result with the source afterwards and report a quality score.
    pub measure_quality: bool,
}

/// The source as the encoder sees it after cutting, cropping and speed changes.
pub fn out_info_for(info: &MediaInfo, start: f64, end: f64, edit: &Edit) -> MediaInfo {
    let mut out = info.clone();
    if let Some(c) = edit.effective_crop(info.width, info.height) {
        out.width = c.w;
        out.height = c.h;
    }
    out.duration = edit.output_duration(start, end);
    out.fps = info.fps * edit.speed();
    out
}

impl JobSpec {
    /// Length of the finished video, after cuts and speed changes.
    pub fn output_duration(&self) -> f64 {
        self.edit.output_duration(self.start, self.end)
    }

    /// The source as the encoder sees it after cutting, cropping and speed changes:
    /// what resolution and frame rate planning works from.
    pub fn out_info(&self) -> MediaInfo {
        out_info_for(&self.info, self.start, self.end, &self.edit)
    }

    fn audio_plan(&self, caps: &Caps) -> Option<AudioPlan> {
        (audio_tracks_out(&self.settings, &self.info) > 0).then_some(AudioPlan {
            source_tracks: self.info.audio_tracks,
            layout: match self.settings.tracks {
                TrackMode::Mix => AudioLayout::Mix,
                TrackMode::First => AudioLayout::First,
                TrackMode::KeepAll => AudioLayout::KeepAll,
            },
            normalize: self.settings.normalize && caps.loudnorm,
        })
    }

    fn tonemap(&self, caps: &Caps) -> Option<&HdrInfo> {
        if self.settings.hdr_to_sdr && caps.tonemap {
            self.info.hdr.as_ref()
        } else {
            None
        }
    }
}

/// Per-frame filters: crop, frame rate, size, denoise, HDR conversion, caption. `pixfmt`
/// ends the chain (None leaves the format to the caller). `denoise` is off for the
/// quality check's reference, so a denoised result is judged against the real source.
fn picture_filters(spec: &JobSpec, caps: &Caps, plan: &Plan, denoise: bool) -> Vec<String> {
    let mut f: Vec<String> = Vec::new();
    let out = spec.out_info();
    if let Some(c) = spec.edit.effective_crop(spec.info.width, spec.info.height) {
        f.push(format!("crop={}:{}:{}:{}", c.w, c.h, c.x, c.y));
    }
    if plan.fps_filter {
        f.push(format!("fps={:.6}", plan.fps));
    }
    if (plan.width, plan.height) != (out.width, out.height) {
        f.push(format!("scale={}:{}:flags=lanczos", plan.width, plan.height));
    }
    if denoise && caps.denoise {
        if let Some(d) = spec.settings.denoise.filter() {
            f.push(d.to_string());
        }
    }
    if let Some(hdr) = spec.tonemap(caps) {
        f.push(tonemap_chain(hdr));
    }
    if caps.drawtext {
        if let Some(c) = edit::caption_filter(&spec.edit, plan.width, plan.height, edit::caption_font().as_deref()) {
            f.push(c);
        }
    }
    f
}

fn video_filter_chain(spec: &JobSpec, caps: &Caps, plan: &Plan, gpu: Option<&GpuEncoder>) -> String {
    let mut f = picture_filters(spec, caps, plan, true);
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
    // -t before -i limits how much of the input is read; as an output option it would
    // cut slow-motion output short.
    push("-ss");
    push(&format!("{:.3}", spec.start));
    push("-t");
    push(&format!("{:.3}", duration));
    push("-i");
    a.push(spec.input.clone().into_os_string());
    let mut push = |v: &str| a.push(OsString::from(v));

    let audio_plan = if pass == Pass::First { None } else { spec.audio_plan(caps) };
    if edit::needs_graph(&spec.edit, spec.start, spec.end, audio_plan) {
        let graph = edit::build_graph(&GraphSpec {
            input: 0,
            video_index: vi,
            start: spec.start,
            end: spec.end,
            edit: &spec.edit,
            vf: vf.clone(),
            audio: audio_plan,
        });
        push("-filter_complex");
        push(&graph.filter);
        push("-map");
        push(&graph.video);
        for label in &graph.audio {
            push("-map");
            push(label);
        }
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

    for v in caps.passthrough_args() {
        push(v);
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
pub(crate) enum RunError {
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

pub(crate) fn run_ffmpeg(
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
    if std::env::var_os("CLIPCRAM_DEBUG").is_some() {
        let shown: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        eprintln!("[clipcram] ffmpeg {}", shown.join(" "));
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
        /// SSIM against the source (1.0 is identical), when measured.
        quality: Option<f64>,
        /// The streams were copied without re-encoding.
        copied: bool,
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

/// Runs a job on a background thread. `wake` is called after every message (the app
/// uses it to repaint).
pub fn spawn(tools: Tools, caps: Arc<Caps>, spec: JobSpec, wake: impl Fn() + Send + 'static) -> Worker {
    let (tx, rx) = mpsc::channel();
    let cancel = CancelToken::default();
    let item_id = spec.item_id;
    let token = cancel.clone();
    thread::spawn(move || {
        let send = |msg: WorkerMsg| {
            let _ = tx.send(msg);
            wake();
        };
        send(finish_message(run_job(&tools, &caps, &spec, &token, &send)));
    });
    Worker { item_id, rx, cancel }
}

/// Runs a job on the calling thread (for the command-line mode).
pub fn run_blocking(tools: &Tools, caps: &Caps, spec: &JobSpec, send: &dyn Fn(WorkerMsg)) -> WorkerMsg {
    finish_message(run_job(tools, caps, spec, &CancelToken::default(), send))
}

fn finish_message(result: Result<Finished, RunError>) -> WorkerMsg {
    match result {
        Ok(f) => WorkerMsg::Done { path: f.path, size: f.size, quality: f.quality, copied: f.copied },
        Err(RunError::Cancelled) => WorkerMsg::Cancelled,
        Err(RunError::Failed(e)) => WorkerMsg::Failed(e),
    }
}

struct Finished {
    path: PathBuf,
    size: u64,
    quality: Option<f64>,
    copied: bool,
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
    let duration = spec.output_duration();
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
) -> Result<Finished, RunError> {
    if spec.end - spec.start <= 0.0 {
        return Err(RunError::Failed("End time must be greater than start time.".to_string()));
    }
    let duration = spec.output_duration();
    if duration < edit::MIN_PART {
        return Err(RunError::Failed("Nothing is left of this clip after the cuts.".to_string()));
    }
    std::fs::create_dir_all(&spec.output_dir).map_err(|e| {
        RunError::Failed(format!("Could not create:\n\n{}\n\n{e}", spec.output_dir.display()))
    })?;
    let tmp = TempDir::new().map_err(|e| RunError::Failed(format!("Could not create a temporary folder: {e}")))?;
    if spec.edit.has_caption() {
        std::fs::write(tmp.0.join("caption.txt"), spec.edit.text.trim())
            .map_err(|e| RunError::Failed(format!("Could not write the caption: {e}")))?;
    }

    if spec.settings.format != OutputFormat::Mp4 {
        let (path, size) = crate::animated::run(tools, caps, spec, &tmp.0, cancel, send)?;
        return Ok(Finished { path, size, quality: None, copied: false });
    }
    if let Some((path, size)) = try_stream_copy(tools, caps, spec, &tmp.0, cancel, send)? {
        return Ok(Finished { path, size, quality: None, copied: true });
    }
    let (path, size, plan) = encode_mp4(tools, caps, spec, &tmp, cancel, send)?;
    let quality = if spec.measure_quality {
        send(WorkerMsg::Status("Measuring quality against the source".to_string()));
        measure_ssim(tools, caps, spec, &plan, &path, &tmp.0, cancel)
    } else {
        None
    };
    Ok(Finished { path, size, quality, copied: false })
}

/// The two-pass/single-pass encode with size checks and retries.
fn encode_mp4(
    tools: &Tools,
    caps: &Caps,
    spec: &JobSpec,
    tmp: &TempDir,
    cancel: &CancelToken,
    send: &dyn Fn(WorkerMsg),
) -> Result<(PathBuf, u64, Plan), RunError> {
    let duration = spec.output_duration();
    let out_info = spec.out_info();

    let codec = spec.settings.codec;
    let two_pass = is_two_pass(codec, caps);
    let target = spec.target_bytes;
    let mut bitrate = initial_bitrate(target, duration, audio_bps(&spec.settings, &spec.info), safety(two_pass));
    let mut last_plan: Option<Plan> = None;
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
        let plan = locked_plan.unwrap_or_else(|| plan(&spec.settings, &out_info, bitrate));
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
            last_plan = Some(plan);
            if let Some((old, _)) = best.replace((partial.clone(), size)) {
                let _ = std::fs::remove_file(old);
            }
            // Use the space that's left by encoding again at a higher bitrate. Two-pass
            // encodes re-run only the second pass, reusing the first-pass analysis.
            let room = size < (target as f64 * 0.90) as u64;
            if fills_leftover_space(codec, spec.settings.speed, two_pass)
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
            "Could not get the output below {} after five encoding attempts.",
            util::format_size(spec.target_bytes as f64)
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
    let plan = last_plan.unwrap_or_else(|| plan(&spec.settings, &out_info, bitrate));
    Ok((final_path, size, plan))
}

/// Copies the streams as they are when that already gives a file Discord plays inline and
/// that fits: no quality loss and almost instant. Cuts land on keyframes, so the clip may
/// start a moment before the chosen start point.
fn try_stream_copy(
    tools: &Tools,
    caps: &Caps,
    spec: &JobSpec,
    tmp: &Path,
    cancel: &CancelToken,
    send: &dyn Fn(WorkerMsg),
) -> Result<Option<(PathBuf, u64)>, RunError> {
    if !copy_eligible(spec, caps) {
        return Ok(None);
    }
    send(WorkerMsg::Status("Already small enough: copying without re-encoding".to_string()));
    let name = util::sanitize_filename(&spec.output_name);
    let partial = spec.output_dir.join(format!(".{name}.copy"));
    let _guard = Partials(vec![partial.clone()]);
    let tracks_out = audio_tracks_out(&spec.settings, &spec.info);
    let mut args: Vec<OsString> = Vec::new();
    for v in ["-hide_banner", "-nostdin", "-y", "-v", "error", "-nostats", "-progress", "pipe:1", "-ss"] {
        args.push(v.into());
    }
    args.push(format!("{:.3}", spec.start).into());
    args.push("-t".into());
    args.push(format!("{:.3}", spec.end - spec.start).into());
    args.push("-i".into());
    args.push(spec.input.clone().into_os_string());
    args.push("-map".into());
    args.push(format!("0:{}", spec.info.video_index).into());
    match (tracks_out, spec.settings.tracks) {
        (0, _) => args.push("-an".into()),
        (_, TrackMode::KeepAll) => args.extend(["-map".into(), "0:a".into()]),
        _ => args.extend(["-map".into(), "0:a:0".into()]),
    }
    for v in ["-c", "copy", "-avoid_negative_ts", "make_zero"] {
        args.push(v.into());
    }
    if spec.settings.codec.family == Family::H265 {
        args.extend(["-tag:v".into(), "hvc1".into()]);
    }
    for v in ["-movflags", "+faststart", "-f", "mp4"] {
        args.push(v.into());
    }
    args.push(partial.clone().into_os_string());
    let mut ignore = |_: f64, _: Option<f64>| {};
    if run_ffmpeg(tools, &args, tmp, cancel, spec.end - spec.start, &mut ignore).is_err() {
        if cancel.is_cancelled() {
            return Err(RunError::Cancelled);
        }
        return Ok(None);
    }
    let size = std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(u64::MAX);
    if size > spec.target_bytes || size == 0 {
        send(WorkerMsg::Status("The copy came out too large, so encoding instead".to_string()));
        return Ok(None);
    }
    let final_path = util::unique_output_path(&spec.output_dir, &name);
    std::fs::rename(&partial, &final_path)
        .or_else(|_| std::fs::copy(&partial, &final_path).map(|_| ()))
        .map_err(|e| RunError::Failed(format!("Could not save {}: {e}", final_path.display())))?;
    Ok(Some((final_path, size)))
}

/// Whether the source can be copied as-is: nothing to change, a codec and pixel format
/// Discord plays, AAC audio, and a size that's likely to fit.
pub fn copy_eligible(spec: &JobSpec, caps: &Caps) -> bool {
    let s = &spec.settings;
    let info = &spec.info;
    let family_matches = matches!(
        (s.codec.family, info.video_codec.as_str()),
        (Family::H264, "h264") | (Family::H265, "hevc") | (Family::Av1, "av1")
    );
    let pix_ok = s.codec.family == Family::Av1 || matches!(info.pix_fmt.as_str(), "yuv420p" | "yuvj420p");
    let short = info.width.min(info.height);
    let size_ok = match s.resolution {
        ResolutionMode::Short(target) => target >= short,
        _ => true,
    };
    let fps_ok = match s.fps {
        FpsMode::Max30 => info.fps <= 30.5,
        FpsMode::Max60 => info.fps <= 60.5,
        _ => true,
    };
    let tracks_out = audio_tracks_out(s, info);
    let audio_ok = tracks_out == 0
        || (info.audio_codecs.iter().take(tracks_out).all(|c| c == "aac")
            && !(s.tracks == TrackMode::Mix && info.audio_tracks > 1));
    let source_secs = info.duration.max(0.01);
    let likely_size = info.size_bytes as f64 * (spec.end - spec.start) / source_secs;
    spec.copy_if_fits
        && s.format == OutputFormat::Mp4
        && spec.edit.is_plain(spec.start, spec.end, info.width, info.height)
        && !s.normalize
        && s.denoise == Denoise::Off
        && family_matches
        && pix_ok
        && spec.tonemap(caps).is_none()
        && size_ok
        && fps_ok
        && audio_ok
        && info.size_bytes > 0
        && likely_size <= spec.target_bytes as f64 * 1.1
}

/// SSIM of the result against the source, prepared the same way (cut, cropped, scaled)
/// but not denoised or compressed. 1.0 means identical; above about 0.95 most people can't
/// tell them apart at normal viewing size.
fn measure_ssim(
    tools: &Tools,
    caps: &Caps,
    spec: &JobSpec,
    plan: &Plan,
    output: &Path,
    tmp: &Path,
    cancel: &CancelToken,
) -> Option<f64> {
    let mut vf = picture_filters(spec, caps, plan, false);
    vf.push(format!("scale={}:{}:flags=lanczos", plan.width, plan.height));
    vf.push("format=yuv420p".to_string());
    let graph = edit::build_graph(&GraphSpec {
        input: 1,
        video_index: spec.info.video_index,
        start: spec.start,
        end: spec.end,
        edit: &spec.edit,
        vf: vf.join(","),
        audio: None,
    });
    let filter = format!(
        "{};[0:v]format=yuv420p,setpts=PTS-STARTPTS[o];{}setpts=PTS-STARTPTS[r];[o][r]ssim",
        graph.filter, graph.video
    );
    let mut args: Vec<OsString> = Vec::new();
    for v in ["-hide_banner", "-nostdin", "-v", "info", "-nostats", "-i"] {
        args.push(v.into());
    }
    args.push(output.as_os_str().to_owned());
    args.push("-ss".into());
    args.push(format!("{:.3}", spec.start).into());
    args.push("-t".into());
    args.push(format!("{:.3}", spec.end - spec.start).into());
    args.push("-i".into());
    args.push(spec.input.clone().into_os_string());
    for v in ["-filter_complex", &filter, "-an", "-f", "null", NULL_DEVICE] {
        args.push(v.into());
    }
    let text = run_for_log(tools, &args, tmp, cancel)?;
    parse_ssim(&text)
}

/// Runs FFmpeg and returns its log, or None if it failed or was cancelled.
fn run_for_log(tools: &Tools, args: &[OsString], cwd: &Path, cancel: &CancelToken) -> Option<String> {
    let mut child = ffmpeg::command(&tools.ffmpeg)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let stderr = child.stderr.take();
    let reader = thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut s) = stderr {
            let _ = std::io::Read::read_to_string(&mut s, &mut text);
        }
        text
    });
    cancel.set_child(child);
    let status = cancel.wait_child();
    let text = reader.join().unwrap_or_default();
    match status {
        Some(Ok(st)) if st.success() && !cancel.is_cancelled() => Some(text),
        _ => None,
    }
}

/// Reads "All:0.975" from FFmpeg's SSIM summary line.
pub fn parse_ssim(log: &str) -> Option<f64> {
    let line = log.lines().rev().find(|l| l.contains("SSIM") && l.contains("All:"))?;
    let value = line.split("All:").nth(1)?.split_whitespace().next()?;
    value.parse::<f64>().ok().filter(|v| (0.0..=1.0).contains(v))
}

/// A plain-words verdict for an SSIM score.
pub fn quality_label(ssim: f64) -> &'static str {
    if ssim >= 0.97 {
        "Excellent"
    } else if ssim >= 0.94 {
        "Good"
    } else if ssim >= 0.90 {
        "Fair"
    } else {
        "Soft"
    }
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

    #[test]
    fn settings_survive_a_save_and_load() {
        let settings = EncodeSettings {
            codec: Codec::on(Engine { backend: Backend::Nvenc, vendor: Vendor::Nvidia, slot: 0 }, Family::Av1),
            resolution: ResolutionMode::Short(720),
            fps: FpsMode::Max30,
            speed: Speed::Best,
            audio: AudioMode::Off,
            tracks: TrackMode::KeepAll,
            hdr_to_sdr: false,
            format: OutputFormat::Webp,
            denoise: Denoise::Light,
            normalize: true,
        };
        let json = serde_json::to_string(&settings).unwrap();
        assert_eq!(serde_json::from_str::<EncodeSettings>(&json).unwrap(), settings);
    }

    #[test]
    fn missing_settings_fields_fall_back_to_defaults() {
        // A settings file written before a field existed keeps what it does have.
        let loaded: EncodeSettings = serde_json::from_str(r#"{"speed":"Best"}"#).unwrap();
        assert_eq!(loaded, EncodeSettings { speed: Speed::Best, ..EncodeSettings::default() });
    }

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
            ..MediaInfo::default()
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
                edit: Edit::default(),
                copy_if_fits: false,
                measure_quality: false,
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
                Ok(Finished { path, size, .. }) => {
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

    fn amd(backend: Backend, tuned: bool) -> GpuEncoder {
        let (encoder, device) = match backend {
            Backend::Vaapi => ("hevc_vaapi", Some("/dev/dri/renderD128".to_string())),
            _ => ("hevc_amf", None),
        };
        GpuEncoder {
            codec: Codec::on(Engine { backend, vendor: Vendor::Amd, slot: 0 }, Family::H265),
            encoder: encoder.to_string(),
            device,
            name: "AMD Radeon RX 7800 XT".to_string(),
            tuned,
        }
    }

    fn gpu_args(gpu: &GpuEncoder, speed: Speed) -> String {
        video_codec_args(gpu.codec, None, Some(gpu), speed, 2_000_000, Pass::Single).unwrap().join(" ")
    }

    #[test]
    fn amd_quality_settings_and_fallback() {
        let tuned = gpu_args(&amd(Backend::Amf, true), Speed::Best);
        assert!(tuned.contains("-quality high_quality -rc hqvbr"));
        assert!(tuned.contains("-preanalysis 1"));
        // The fallback is exactly the old, widely supported setup.
        let plain = gpu_args(&amd(Backend::Amf, false), Speed::Best);
        assert!(plain.contains("-quality quality -rc vbr_peak"));
        assert!(!plain.contains("preanalysis"));

        assert!(gpu_args(&amd(Backend::Vaapi, true), Speed::Balanced).contains("-compression_level 1"));
        assert!(!gpu_args(&amd(Backend::Vaapi, false), Speed::Balanced).contains("compression_level"));
    }

    fn vendor_gpu(backend: Backend, family: Family, encoder: &str, tuned: bool) -> GpuEncoder {
        GpuEncoder {
            codec: Codec::on(Engine { backend, vendor: backend.native_vendor(), slot: 0 }, family),
            encoder: encoder.to_string(),
            device: None,
            name: "GPU".to_string(),
            tuned,
        }
    }

    #[test]
    fn every_vendor_has_quality_settings_with_a_plain_fallback() {
        let cases = [
            (Backend::Nvenc, "hevc_nvenc", "-b_ref_mode middle"),
            (Backend::Qsv, "hevc_qsv", "-extbrc 1 -look_ahead_depth 40"),
            (Backend::VideoToolbox, "hevc_videotoolbox", "-prio_speed 0"),
        ];
        for (backend, encoder, extra) in cases {
            let tuned = gpu_args(&vendor_gpu(backend, Family::H265, encoder, true), Speed::Balanced);
            let plain = gpu_args(&vendor_gpu(backend, Family::H265, encoder, false), Speed::Balanced);
            assert!(tuned.contains(extra), "{backend:?}: {tuned}");
            assert!(!plain.contains(extra), "{backend:?}: {plain}");
        }
    }

    #[test]
    fn gpu_rate_cap_leaves_room_for_action() {
        let a = gpu_args(&amd(Backend::Vaapi, true), Speed::Balanced);
        assert!(a.contains("-maxrate 6000000 -bufsize 6000000"));
    }

    #[test]
    fn which_encodes_fill_leftover_space() {
        let gpu = Codec::on(Engine { backend: Backend::Vaapi, vendor: Vendor::Amd, slot: 0 }, Family::H265);
        assert!(fills_leftover_space(gpu, Speed::Balanced, false));
        assert!(fills_leftover_space(gpu, Speed::Best, false));
        assert!(!fills_leftover_space(gpu, Speed::Faster, false));
        // CPU encodes keep their old behavior.
        assert!(fills_leftover_space(Codec::H265, Speed::Best, true));
        assert!(!fills_leftover_space(Codec::H265, Speed::Balanced, true));
        assert!(!fills_leftover_space(Codec::AV1, Speed::Best, false));
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
