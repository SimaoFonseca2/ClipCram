//! Finding FFmpeg, detecting what it can do, and reading video information.

use crate::encode::{self, Backend, Codec, Family, Pass, Speed};
use serde_json::Value;
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Where pass-1 output goes. FFmpeg's null muxer writes nothing, but it still wants a target.
pub const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

// ============================================================
// TOOLS
// ============================================================

#[derive(Clone, Debug)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Tools {
    /// Looks next to the executable first (so FFmpeg can be bundled), then on PATH,
    /// then in common install locations that GUI apps on macOS don't get on PATH.
    pub fn find() -> Result<Tools, Vec<&'static str>> {
        let ffmpeg = find_program("ffmpeg");
        let ffprobe = find_program("ffprobe");
        match (ffmpeg, ffprobe) {
            (Some(ffmpeg), Some(ffprobe)) => Ok(Tools { ffmpeg, ffprobe }),
            (a, b) => {
                let mut missing = Vec::new();
                if a.is_none() {
                    missing.push("ffmpeg");
                }
                if b.is_none() {
                    missing.push("ffprobe");
                }
                Err(missing)
            }
        }
    }
}

fn find_program(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };

    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        for candidate in [
            dir.join(&file),
            dir.join("ffmpeg").join(&file),
            dir.join("ffmpeg").join("bin").join(&file),
        ] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(&file);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let extra: &[&str] = if cfg!(target_os = "macos") {
        &["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"]
    } else if cfg!(windows) {
        &[]
    } else {
        &["/usr/local/bin", "/usr/bin", "/snap/bin"]
    };
    extra
        .iter()
        .map(|d| Path::new(d).join(&file))
        .find(|c| c.is_file())
}

/// A `Command` that never pops up a console window on Windows and keeps SVT-AV1 quiet.
pub fn command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null());
    cmd.env("SVT_LOG", "1");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

// ============================================================
// CAPABILITIES
// ============================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Av1Encoder {
    Svt,
    Aom,
    Rav1e,
}

impl Av1Encoder {
    pub fn name(self) -> &'static str {
        match self {
            Av1Encoder::Svt => "libsvtav1",
            Av1Encoder::Aom => "libaom-av1",
            Av1Encoder::Rav1e => "librav1e",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GpuEncoder {
    pub codec: Codec,
    pub encoder: String,
    /// VA-API render node, e.g. `/dev/dri/renderD128`.
    pub device: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Caps {
    pub x264: bool,
    pub x265: bool,
    pub av1: Option<Av1Encoder>,
    /// `aac_at` (Apple's encoder, better at low bitrates) when present, otherwise FFmpeg's `aac`.
    pub aac: String,
    /// `zscale` + `tonemap` filters are available for HDR to SDR conversion.
    pub tonemap: bool,
    pub gpu: Vec<GpuEncoder>,
    /// A hardware decoder that passed a test decode, used to speed up reading the source.
    pub hwaccel: Option<String>,
    pub hardware_checked: bool,
    encoders: HashSet<String>,
    hwaccels: Vec<String>,
}

impl Caps {
    /// Fast checks that run at startup (two short FFmpeg calls).
    pub fn detect(tools: &Tools) -> Caps {
        let encoders = list_names(&tools.ffmpeg, "-encoders");
        let filters = list_names(&tools.ffmpeg, "-filters");
        let hwaccels = list_hwaccels(&tools.ffmpeg);

        let av1 = if encoders.contains("libsvtav1") {
            Some(Av1Encoder::Svt)
        } else if encoders.contains("libaom-av1") {
            Some(Av1Encoder::Aom)
        } else if encoders.contains("librav1e") {
            Some(Av1Encoder::Rav1e)
        } else {
            None
        };

        Caps {
            x264: encoders.contains("libx264"),
            x265: encoders.contains("libx265"),
            av1,
            aac: if encoders.contains("aac_at") { "aac_at" } else { "aac" }.to_string(),
            tonemap: filters.contains("zscale") && filters.contains("tonemap"),
            gpu: Vec::new(),
            hwaccel: None,
            hardware_checked: false,
            encoders,
            hwaccels,
        }
    }

    /// Software codecs in order of preference (H.265, H.264, AV1), then any working GPU encoders.
    pub fn codec_options(&self) -> Vec<Codec> {
        let mut out = Vec::new();
        if self.x265 {
            out.push(Codec::H265);
        }
        if self.x264 {
            out.push(Codec::H264);
        }
        if self.av1.is_some() {
            out.push(Codec::AV1);
        }
        out.extend(self.gpu.iter().map(|g| g.codec));
        out
    }

    /// Default codec: H.265 if possible, then H.264, then AV1.
    pub fn default_codec(&self) -> Codec {
        if self.x265 {
            Codec::H265
        } else if self.x264 {
            Codec::H264
        } else if self.av1.is_some() {
            Codec::AV1
        } else {
            self.gpu.first().map(|g| g.codec).unwrap_or(Codec::H264)
        }
    }

    pub fn gpu_for(&self, codec: Codec) -> Option<&GpuEncoder> {
        self.gpu.iter().find(|g| g.codec == codec)
    }

    pub fn has_any_encoder(&self) -> bool {
        self.x264 || self.x265 || self.av1.is_some() || !self.gpu.is_empty()
    }
}

fn run_capture(program: &Path, args: &[&str]) -> String {
    command(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Parses `ffmpeg -encoders` / `ffmpeg -filters`: a flags column, then the name.
fn list_names(ffmpeg: &Path, flag: &str) -> HashSet<String> {
    let text = run_capture(ffmpeg, &["-hide_banner", flag]);
    let mut names = HashSet::new();
    let mut past_header = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("---") || trimmed.starts_with("------") {
            past_header = true;
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        if let (Some(flags), Some(name)) = (parts.next(), parts.next()) {
            // Encoder flags look like "V....D"; filter flags like "..." or "TSC".
            let looks_like_flags = flags.chars().all(|c| c == '.' || c.is_ascii_uppercase());
            if (past_header || flag == "-filters") && looks_like_flags && !flags.is_empty() {
                names.insert(name.to_string());
            }
        }
    }
    names
}

fn list_hwaccels(ffmpeg: &Path) -> Vec<String> {
    run_capture(ffmpeg, &["-hide_banner", "-hwaccels"])
        .lines()
        .skip_while(|l| !l.contains("Hardware acceleration methods"))
        .skip(1)
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Runs a command and kills it if it takes too long. Returns (succeeded, stderr).
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> (bool, String) {
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return (false, String::new()),
    };
    let mut stderr = child.stderr.take();
    let reader = thread::spawn(move || {
        let mut text = String::new();
        if let Some(s) = stderr.as_mut() {
            let _ = s.read_to_string(&mut text);
        }
        text
    });
    let started = Instant::now();
    let ok = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            Ok(None) => thread::sleep(Duration::from_millis(40)),
            Err(_) => break false,
        }
    };
    (ok, reader.join().unwrap_or_default())
}

const GPU_CANDIDATES: &[(Backend, Family, &str)] = &[
    (Backend::Nvenc, Family::H264, "h264_nvenc"),
    (Backend::Nvenc, Family::H265, "hevc_nvenc"),
    (Backend::Nvenc, Family::Av1, "av1_nvenc"),
    (Backend::Amf, Family::H264, "h264_amf"),
    (Backend::Amf, Family::H265, "hevc_amf"),
    (Backend::Amf, Family::Av1, "av1_amf"),
    (Backend::Qsv, Family::H264, "h264_qsv"),
    (Backend::Qsv, Family::H265, "hevc_qsv"),
    (Backend::Qsv, Family::Av1, "av1_qsv"),
    (Backend::VideoToolbox, Family::H264, "h264_videotoolbox"),
    (Backend::VideoToolbox, Family::H265, "hevc_videotoolbox"),
    (Backend::Vaapi, Family::H264, "h264_vaapi"),
    (Backend::Vaapi, Family::H265, "hevc_vaapi"),
    (Backend::Vaapi, Family::Av1, "av1_vaapi"),
];

fn vaapi_devices() -> Vec<String> {
    let mut devices: Vec<String> = std::fs::read_dir("/dev/dri")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().starts_with("renderD"))
                        .unwrap_or(false)
                })
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    devices.sort();
    devices
}

/// Encodes half a second of a test pattern with exactly the arguments a real job would use,
/// so an encoder only shows up in the list if it actually works on this machine.
fn gpu_encoder_works(tools: &Tools, gpu: &GpuEncoder) -> bool {
    let mut cmd = command(&tools.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-v", "error"]);
    cmd.args(encode::gpu_pre_input_args(gpu));
    cmd.args(["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30", "-t", "0.5"]);
    let mut filters = vec!["format=nv12".to_string()];
    if let Some(tail) = encode::gpu_filter_tail(gpu) {
        filters.push(tail.to_string());
    }
    cmd.args(["-vf", &filters.join(",")]);
    match encode::video_codec_args(gpu.codec, None, Some(gpu), Speed::Balanced, 2_000_000, Pass::Single) {
        Ok(args) => {
            cmd.args(args);
        }
        Err(_) => return false,
    }
    cmd.args(["-an", "-f", "null", NULL_DEVICE]);
    run_with_timeout(cmd, Duration::from_secs(20)).0
}

/// Slower checks that run in the background after the window opens:
/// which GPU encoders really work, and whether hardware decoding works.
pub fn detect_hardware(tools: &Tools, caps: &Caps) -> (Vec<GpuEncoder>, Option<String>) {
    let mut found: Vec<GpuEncoder> = Vec::new();

    for &(backend, family, name) in GPU_CANDIDATES {
        if !caps.encoders.contains(name) {
            continue;
        }
        let codec = Codec { family, backend };
        if backend == Backend::Vaapi {
            for device in vaapi_devices() {
                let gpu = GpuEncoder {
                    codec,
                    encoder: name.to_string(),
                    device: Some(device),
                };
                if gpu_encoder_works(tools, &gpu) {
                    found.push(gpu);
                    break;
                }
            }
        } else {
            let gpu = GpuEncoder {
                codec,
                encoder: name.to_string(),
                device: None,
            };
            if gpu_encoder_works(tools, &gpu) {
                found.push(gpu);
            }
        }
    }

    // Prefer one backend per codec family in this order; a VA-API entry only shows up
    // when no vendor-specific encoder handles that family.
    let mut deduped: Vec<GpuEncoder> = Vec::new();
    for gpu in found {
        if gpu.codec.backend == Backend::Vaapi
            && deduped.iter().any(|g| g.codec.family == gpu.codec.family)
        {
            continue;
        }
        deduped.push(gpu);
    }

    let hwaccel = detect_hwaccel(tools, caps, &deduped);
    (deduped, hwaccel)
}

fn detect_hwaccel(tools: &Tools, caps: &Caps, gpu: &[GpuEncoder]) -> Option<String> {
    let preferred: &[&str] = if cfg!(target_os = "macos") {
        &["videotoolbox"]
    } else if cfg!(windows) {
        &["cuda", "d3d11va", "dxva2"]
    } else {
        &["cuda", "vaapi"]
    };
    let candidates: Vec<&str> = preferred
        .iter()
        .copied()
        .filter(|h| caps.hwaccels.iter().any(|a| a == h))
        .collect();
    if candidates.is_empty() {
        return None;
    }

    // Make a tiny H.264 file to decode. Without an H.264 encoder there's nothing to test with.
    let encoder = if caps.x264 {
        "libx264".to_string()
    } else if let Some(g) = gpu.iter().find(|g| g.codec.family == Family::H264) {
        g.encoder.clone()
    } else {
        return None;
    };
    let dir = std::env::temp_dir().join(format!("clipcram-hwtest-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let sample = dir.join("sample.mp4");
    let mut make = command(&tools.ffmpeg);
    make.args(["-hide_banner", "-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=1280x720:rate=30")
        .args(["-t", "1", "-c:v", &encoder, "-pix_fmt", "yuv420p"])
        .arg(&sample);
    let made = run_with_timeout(make, Duration::from_secs(20)).0;

    let mut chosen = None;
    if made {
        for accel in candidates {
            let mut cmd = command(&tools.ffmpeg);
            cmd.args(["-hide_banner", "-nostdin", "-v", "error", "-hwaccel", accel, "-i"])
                .arg(&sample)
                .args(["-f", "null", NULL_DEVICE]);
            let (ok, stderr) = run_with_timeout(cmd, Duration::from_secs(20));
            if ok && stderr.trim().is_empty() {
                chosen = Some(accel.to_string());
                break;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    chosen
}

// ============================================================
// PROBING
// ============================================================

#[derive(Clone, Debug, PartialEq)]
pub struct HdrInfo {
    pub transfer: String,
    pub primaries: String,
    pub matrix: String,
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub duration: f64,
    /// Display size, with phone rotation metadata already applied.
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Absolute stream index of the main video stream (skips embedded cover art).
    pub video_index: usize,
    pub audio_tracks: usize,
    pub hdr: Option<HdrInfo>,
    pub video_codec: String,
}

fn parse_rate(value: Option<&Value>) -> Option<f64> {
    let s = value?.as_str()?;
    let (a, b) = s.split_once('/').unwrap_or((s, "1"));
    let (a, b): (f64, f64) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    if b == 0.0 {
        return None;
    }
    let fps = a / b;
    (fps.is_finite() && fps > 0.5 && fps < 1000.0).then_some(fps)
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub fn probe(tools: &Tools, path: &Path) -> Result<MediaInfo, String> {
    let output = command(&tools.ffprobe)
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Could not run FFprobe: {e}"))?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "FFprobe could not read this file.".to_string()
        } else {
            err
        });
    }

    let json: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "FFprobe returned output that could not be read.".to_string())?;
    let streams = json["streams"].as_array().cloned().unwrap_or_default();

    let video = streams
        .iter()
        .find(|s| {
            s["codec_type"] == "video" && s["disposition"]["attached_pic"].as_i64().unwrap_or(0) == 0
        })
        .ok_or_else(|| "Could not determine video resolution.".to_string())?;

    let raw_w = video["width"].as_u64().unwrap_or(0) as u32;
    let raw_h = video["height"].as_u64().unwrap_or(0) as u32;
    if raw_w == 0 || raw_h == 0 {
        return Err("Could not determine video resolution.".to_string());
    }

    // Phones store portrait video as landscape plus a rotation flag; FFmpeg rotates on decode,
    // so the size we scale to has to be the rotated one.
    let mut rotation = video["side_data_list"]
        .as_array()
        .and_then(|list| list.iter().find_map(|sd| number(sd.get("rotation"))))
        .or_else(|| number(video["tags"].get("rotate")))
        .unwrap_or(0.0)
        .round() as i64;
    rotation = rotation.rem_euclid(360);
    let (width, height) = if rotation == 90 || rotation == 270 {
        (raw_h, raw_w)
    } else {
        (raw_w, raw_h)
    };

    let fps = parse_rate(video.get("avg_frame_rate"))
        .or_else(|| parse_rate(video.get("r_frame_rate")))
        .unwrap_or(30.0);

    let duration = number(json["format"].get("duration"))
        .or_else(|| number(video.get("duration")))
        .filter(|d| d.is_finite() && *d > 0.0)
        .ok_or_else(|| "FFprobe returned an invalid duration.".to_string())?;

    let audio_tracks = streams.iter().filter(|s| s["codec_type"] == "audio").count();

    let transfer = video["color_transfer"].as_str().unwrap_or("").to_string();
    let hdr = (transfer == "smpte2084" || transfer == "arib-std-b67").then(|| HdrInfo {
        transfer: transfer.clone(),
        primaries: video["color_primaries"]
            .as_str()
            .filter(|s| !s.is_empty() && *s != "unknown")
            .unwrap_or("bt2020")
            .to_string(),
        matrix: video["color_space"]
            .as_str()
            .filter(|s| !s.is_empty() && *s != "unknown")
            .unwrap_or("bt2020nc")
            .to_string(),
    });

    Ok(MediaInfo {
        duration,
        width,
        height,
        fps,
        video_index: video["index"].as_u64().unwrap_or(0) as usize,
        audio_tracks,
        hdr,
        video_codec: video["codec_name"].as_str().unwrap_or("").to_string(),
    })
}
