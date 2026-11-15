//! Finding FFmpeg, detecting what it can do, and reading video information.

use crate::encode::{self, Backend, Codec, Engine, Family, Pass, Speed, Vendor};
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

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GpuEncoder {
    pub codec: Codec,
    pub encoder: String,
    /// VA-API render node, e.g. `/dev/dri/renderD128`.
    pub device: Option<String>,
    /// What the user sees in the "Encode on" list, e.g. "AMD Radeon RX 7800 XT".
    pub name: String,
    /// Uses the extra quality settings for its backend (see `encode::video_codec_args`).
    /// False when those failed the self-test and the plain settings passed instead, which
    /// happens with older FFmpeg builds or drivers that lack some of the options.
    pub tuned: bool,
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
    /// Optional filters some features need.
    /// FFmpeg's major version (5 for 5.x), or 99 for builds from git, which don't say.
    pub ffmpeg_major: u32,
    pub drawtext: bool,
    pub loudnorm: bool,
    pub denoise: bool,
    /// The animated WebP encoder (`libwebp_anim`, or `libwebp` in builds that name it so).
    pub webp: Option<String>,
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
        // Three independent FFmpeg calls; run them side by side so the window opens sooner.
        let (encoders, filters, hwaccels, version) = thread::scope(|s| {
            let encoders = s.spawn(|| list_names(&tools.ffmpeg, "-encoders"));
            let filters = s.spawn(|| list_names(&tools.ffmpeg, "-filters"));
            let version = s.spawn(|| run_capture(&tools.ffmpeg, &["-hide_banner", "-version"]));
            let hwaccels = list_hwaccels(&tools.ffmpeg);
            (
                encoders.join().unwrap_or_default(),
                filters.join().unwrap_or_default(),
                hwaccels,
                version.join().unwrap_or_default(),
            )
        });

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
            ffmpeg_major: parse_major_version(&version),
            drawtext: filters.contains("drawtext"),
            loudnorm: filters.contains("loudnorm"),
            denoise: filters.contains("hqdn3d"),
            webp: ["libwebp_anim", "libwebp"].into_iter().find(|e| encoders.contains(*e)).map(String::from),
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

    /// The codec to use when the user last picked `wanted`: that codec if it works here,
    /// otherwise the default. The flag is true while the answer may still change, because
    /// GPU encoders are detected in the background a few seconds after startup.
    pub fn resolve_codec(&self, wanted: Codec) -> (Codec, bool) {
        let wanted = wanted.normalized();
        if self.codec_options().contains(&wanted) {
            (wanted, false)
        } else {
            let pending = !self.hardware_checked && (wanted.is_gpu() || !self.has_any_encoder());
            (self.default_codec(), pending)
        }
    }

    /// Everywhere a video can be encoded, for the "Encode on" list: the CPU first (when
    /// FFmpeg has a CPU encoder), then each GPU, with the name to show for it.
    pub fn engines(&self) -> Vec<(Engine, String)> {
        let mut out: Vec<(Engine, String)> = Vec::new();
        if self.x264 || self.x265 || self.av1.is_some() {
            out.push((Engine::CPU, "CPU".to_string()));
        }
        for gpu in &self.gpu {
            let engine = gpu.codec.engine();
            if !out.iter().any(|(e, _)| *e == engine) {
                out.push((engine, gpu.name.clone()));
            }
        }
        // The same card can show up twice (AMF and VA-API for one of two AMD cards, say),
        // and two cards can share a model name; add what tells them apart.
        let names: Vec<String> = out.iter().map(|(_, n)| n.clone()).collect();
        for (engine, name) in &mut out {
            if names.iter().filter(|n| *n == name).count() > 1 {
                if engine.backend == Backend::Vaapi {
                    name.push_str(&format!(" (VA-API, renderD{})", 128 + engine.slot as u32));
                } else {
                    name.push_str(&format!(" ({})", engine.backend.api_name()));
                }
            }
        }
        out
    }

    pub fn engine_name(&self, engine: Engine) -> String {
        self.engines()
            .into_iter()
            .find(|(e, _)| *e == engine)
            .map(|(_, n)| n)
            .unwrap_or_else(|| match engine.backend {
                Backend::Software => "CPU".to_string(),
                b => format!("{} GPU", if b == Backend::Vaapi { engine.vendor } else { b.native_vendor() }.label()),
            })
    }

    /// The codecs one engine can encode, in the usual order (H.265, H.264, AV1).
    pub fn codecs_on(&self, engine: Engine) -> Vec<Codec> {
        let mut codecs: Vec<Codec> =
            self.codec_options().into_iter().filter(|c| c.engine() == engine).collect();
        codecs.sort_by_key(|c| match c.family {
            Family::H265 => 0,
            Family::H264 => 1,
            Family::Av1 => 2,
        });
        codecs
    }

    pub fn gpu_for(&self, codec: Codec) -> Option<&GpuEncoder> {
        self.gpu.iter().find(|g| g.codec == codec)
    }

    pub fn has_any_encoder(&self) -> bool {
        self.x264 || self.x265 || self.av1.is_some() || !self.gpu.is_empty()
    }
}

/// "ffmpeg version 6.1.1-3ubuntu5" -> 6, "ffmpeg version n7.1" -> 7; git builds
/// ("N-112345-g...") are newer than any release.
pub fn parse_major_version(text: &str) -> u32 {
    let word = text.split_whitespace().skip_while(|w| *w != "version").nth(1).unwrap_or("");
    let word = word.trim_start_matches('n');
    word.split(['.', '-']).next().and_then(|m| m.parse().ok()).unwrap_or(99)
}

impl Caps {
    /// Options that keep every frame exactly as decoded, with no duplicates added at the
    /// end. Without them a null-output first pass and an MP4 second pass can disagree by a
    /// frame when the audio runs longer than the video, which breaks two-pass encoding.
    pub fn passthrough_args(&self) -> [&'static str; 2] {
        if self.ffmpeg_major >= 6 || self.ffmpeg_major == 5 {
            ["-fps_mode", "passthrough"]
        } else {
            ["-vsync", "passthrough"]
        }
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

/// A GPU as Linux lists it under /sys/class/drm.
#[derive(Clone, Debug, PartialEq)]
pub struct GpuDevice {
    /// The render node, e.g. `/dev/dri/renderD128`.
    pub node: String,
    /// renderD128 is slot 0, renderD129 slot 1, and so on.
    pub slot: u8,
    pub vendor: Vendor,
    pub name: String,
}

/// Every GPU with a render node, in node order. Empty on Windows and macOS, which don't
/// have VA-API; their encoders each drive one vendor's GPU.
fn gpu_devices() -> Vec<GpuDevice> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else { return Vec::new() };
    let mut nodes: Vec<(u32, String)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let minor: u32 = name.strip_prefix("renderD")?.parse().ok()?;
            Some((minor, name))
        })
        .collect();
    nodes.sort();
    let ids = PciIds::load();
    nodes
        .into_iter()
        .map(|(minor, node)| {
            let read_id = |file: &str| {
                std::fs::read_to_string(format!("/sys/class/drm/{node}/device/{file}"))
                    .ok()
                    .and_then(|t| u16::from_str_radix(t.trim().trim_start_matches("0x"), 16).ok())
            };
            let (vendor_id, device_id) = (read_id("vendor"), read_id("device"));
            let vendor = vendor_id.map(Vendor::from_pci_id).unwrap_or(Vendor::Other);
            let model = vendor_id.zip(device_id).and_then(|(v, d)| ids.model(v, d));
            GpuDevice {
                node: format!("/dev/dri/{node}"),
                slot: minor.saturating_sub(128).min(u8::MAX as u32) as u8,
                vendor,
                name: gpu_display_name(vendor, model.as_deref()),
            }
        })
        .collect()
}

/// "AMD Radeon RX 7800 XT" from the vendor and a pci.ids model name such as
/// "Navi 32 [Radeon RX 7700 XT / 7800 XT]" (the marketing name is the bracketed part).
fn gpu_display_name(vendor: Vendor, model: Option<&str>) -> String {
    let marketing = model.map(|m| match (m.find('['), m.rfind(']')) {
        (Some(open), Some(close)) if open < close => m[open + 1..close].trim(),
        _ => m.trim(),
    });
    match (vendor, marketing) {
        (Vendor::Other, Some(m)) => m.to_string(),
        (v, Some(m)) => format!("{} {m}", v.label()),
        (Vendor::Other, None) => "GPU".to_string(),
        (v, None) => format!("{} GPU", v.label()),
    }
}

/// The PCI ID database that `lspci` uses, for turning vendor and device IDs into names.
struct PciIds(String);

impl PciIds {
    fn load() -> PciIds {
        let paths = ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids", "/usr/share/pci.ids"];
        PciIds(paths.iter().find_map(|p| std::fs::read_to_string(p).ok()).unwrap_or_default())
    }

    fn model(&self, vendor: u16, device: u16) -> Option<String> {
        // Vendors start at column 0 ("1002  Advanced Micro Devices"), their devices follow
        // indented by one tab ("\t747e  Navi 32 [...]"), subsystems by two tabs.
        let (vendor_hex, device_hex) = (format!("{vendor:04x}"), format!("{device:04x}"));
        let mut in_vendor = false;
        for line in self.0.lines() {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            if !line.starts_with('\t') {
                if in_vendor {
                    return None;
                }
                in_vendor = line.get(..4).is_some_and(|id| id.eq_ignore_ascii_case(&vendor_hex));
            } else if in_vendor && !line.starts_with("\t\t") {
                let entry = &line[1..];
                if entry.get(..4).is_some_and(|id| id.eq_ignore_ascii_case(&device_hex)) {
                    return Some(entry[4..].trim().to_string());
                }
            }
        }
        None
    }
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
    thread::scope(|s| {
        // The hardware decoding test only needs a GPU encoder to make its sample file when
        // there's no x264, so usually it can run at the same time as the encoder tests.
        let early_hwaccel = caps.x264.then(|| s.spawn(|| detect_hwaccel(tools, caps, &[])));
        let gpu = detect_gpu_encoders(tools, caps);
        let hwaccel = match early_hwaccel {
            Some(handle) => handle.join().ok().flatten(),
            None => detect_hwaccel(tools, caps, &gpu),
        };
        (gpu, hwaccel)
    })
}

/// Test-encodes every GPU encoder this FFmpeg build has, on every GPU. Each vendor API
/// (NVENC, AMF, Quick Sync, VideoToolbox) and each VA-API device gets its own thread, and
/// tests one codec at a time, so different GPUs are checked at the same time.
fn detect_gpu_encoders(tools: &Tools, caps: &Caps) -> Vec<GpuEncoder> {
    let devices = gpu_devices();
    detect_gpu_encoders_with(&devices, caps, |gpu| gpu_encoder_works(tools, gpu))
}

/// The detection logic, with the test encode passed in so tests can stand in for a GPU.
fn detect_gpu_encoders_with(
    devices: &[GpuDevice],
    caps: &Caps,
    works: impl Fn(&GpuEncoder) -> bool + Sync,
) -> Vec<GpuEncoder> {
    let listed = |backend: Backend| {
        GPU_CANDIDATES
            .iter()
            .filter(move |&&(b, _, name)| b == backend && caps.encoders.contains(name))
    };
    // A vendor API addresses "the" GPU of its vendor. Name it after that GPU when there's
    // exactly one, so it reads the same as the VA-API entry for the same card would.
    let only_device_of = |vendor: Vendor| {
        let mut of_vendor = devices.iter().filter(|d| d.vendor == vendor);
        match (of_vendor.next(), of_vendor.next()) {
            (Some(d), None) => Some(d),
            _ => None,
        }
    };
    let works = &works;
    let test = move |codec: Codec, encoder: &str, device: Option<String>, name: String| {
        let mut gpu = GpuEncoder { codec, encoder: encoder.to_string(), device, name, tuned: true };
        if works(&gpu) {
            return Some(gpu);
        }
        gpu.tuned = false;
        works(&gpu).then_some(gpu)
    };

    let vendors = [Backend::Nvenc, Backend::Amf, Backend::Qsv, Backend::VideoToolbox];
    let native: Vec<GpuEncoder> = thread::scope(|s| {
        let handles: Vec<_> = vendors
            .iter()
            .map(|&backend| {
                let vendor = backend.native_vendor();
                let name = only_device_of(vendor)
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| format!("{} GPU", vendor.label()));
                s.spawn(move || {
                    listed(backend)
                        .filter_map(|&(backend, family, encoder)| {
                            let engine = Engine { backend, vendor, slot: 0 };
                            test(Codec::on(engine, family), encoder, None, name.clone())
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        // Joined in vendor order, so the list keeps a stable order.
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });

    // VA-API on each GPU, skipping codecs that GPU's own vendor API already handles (it's
    // the same hardware, and the vendor API has the better quality controls). NVIDIA's
    // VA-API driver only decodes, so NVIDIA cards are skipped.
    let vaapi: Vec<GpuEncoder> = thread::scope(|s| {
        let handles: Vec<_> = devices
            .iter()
            .filter(|d| d.vendor != Vendor::Nvidia)
            .map(|device| {
                let covered_by_native = |family: Family| {
                    only_device_of(device.vendor).is_some()
                        && native.iter().any(|g| g.codec.vendor == device.vendor && g.codec.family == family)
                };
                s.spawn(move || {
                    let engine = Engine { backend: Backend::Vaapi, vendor: device.vendor, slot: device.slot };
                    listed(Backend::Vaapi)
                        .filter(|&&(_, family, _)| !covered_by_native(family))
                        .filter_map(|&(_, family, encoder)| {
                            test(Codec::on(engine, family), encoder, Some(device.node.clone()), device.name.clone())
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });

    let mut found = native;
    found.extend(vaapi);
    found
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

#[derive(Clone, Debug, Default)]
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
    /// Pixel format of the video, e.g. `yuv420p`.
    pub pix_fmt: String,
    /// Codec of each audio track, e.g. `aac`.
    pub audio_codecs: Vec<String>,
    /// Title of each audio track ("Game", "Mic" from OBS), or empty.
    pub audio_titles: Vec<String>,
    /// Size of the source file.
    pub size_bytes: u64,
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

    let audio: Vec<&Value> = streams.iter().filter(|s| s["codec_type"] == "audio").collect();
    let audio_tracks = audio.len();
    let audio_codecs = audio.iter().map(|s| s["codec_name"].as_str().unwrap_or("").to_string()).collect();
    let audio_titles = audio
        .iter()
        .map(|s| {
            let tags = &s["tags"];
            tags["title"]
                .as_str()
                .or_else(|| tags["handler_name"].as_str().filter(|h| !h.contains("Handler")))
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .collect();

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
        pix_fmt: video["pix_fmt"].as_str().unwrap_or("").to_string(),
        audio_codecs,
        audio_titles,
        size_bytes: number(json["format"].get("size"))
            .map(|s| s as u64)
            .or_else(|| std::fs::metadata(path).ok().map(|m| m.len()))
            .unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NVENC: Engine = Engine { backend: Backend::Nvenc, vendor: Vendor::Nvidia, slot: 0 };
    const NVENC_HEVC: Codec = Codec::on(NVENC, Family::H265);

    fn software_only() -> Caps {
        Caps { x264: true, x265: true, ..Caps::default() }
    }

    #[test]
    fn remembered_codec_is_used_when_available() {
        assert_eq!(software_only().resolve_codec(Codec::H264), (Codec::H264, false));
    }

    #[test]
    fn remembered_gpu_codec_waits_for_detection() {
        let caps = software_only();
        assert_eq!(caps.resolve_codec(NVENC_HEVC), (Codec::H265, true));

        let mut checked = caps.clone();
        checked.hardware_checked = true;
        checked.gpu.push(GpuEncoder {
            codec: NVENC_HEVC,
            encoder: "hevc_nvenc".into(),
            device: None,
            name: "NVIDIA GeForce RTX 4070".into(),
            tuned: true,
        });
        assert_eq!(checked.resolve_codec(NVENC_HEVC), (NVENC_HEVC, false));
    }

    #[test]
    fn missing_codec_falls_back_for_good_after_detection() {
        let mut caps = software_only();
        caps.hardware_checked = true;
        assert_eq!(caps.resolve_codec(NVENC_HEVC), (Codec::H265, false));
        // AV1 remembered from another machine, not in this FFmpeg build.
        assert_eq!(caps.resolve_codec(Codec::AV1), (Codec::H265, false));
    }

    fn device(slot: u8, vendor: Vendor, name: &str) -> GpuDevice {
        GpuDevice { node: format!("/dev/dri/renderD{}", 128 + slot as u32), slot, vendor, name: name.into() }
    }

    fn caps_with(encoders: &[&str]) -> Caps {
        let mut caps = software_only();
        caps.encoders = encoders.iter().map(|e| e.to_string()).collect();
        caps
    }

    const ALL_VAAPI: &[&str] = &["h264_vaapi", "hevc_vaapi", "av1_vaapi"];

    fn names(found: &[GpuEncoder]) -> Vec<String> {
        found.iter().map(|g| format!("{} {}", g.name, g.codec.label())).collect()
    }

    #[test]
    fn ffmpeg_versions() {
        assert_eq!(parse_major_version("ffmpeg version 6.1.1-3ubuntu5 Copyright"), 6);
        assert_eq!(parse_major_version("ffmpeg version n7.1 Copyright"), 7);
        assert_eq!(parse_major_version("ffmpeg version 4.4.2-0ubuntu0.22.04.1"), 4);
        assert_eq!(parse_major_version("ffmpeg version N-112345-gabcdef"), 99);
        assert_eq!(parse_major_version(""), 99);
    }

    #[test]
    fn pci_ids_lookup_and_names() {
        let ids = PciIds(
            "# comment\n1002  Advanced Micro Devices, Inc. [AMD/ATI]\n\t747e  Navi 32 [Radeon RX 7700 XT / 7800 XT]\n\t\t1002 0e3b  RX 7800 XT\n\t164e  Raphael\n10de  NVIDIA Corporation\n\t2786  AD104 [GeForce RTX 4070]\n".into(),
        );
        let model = ids.model(0x1002, 0x747e).unwrap();
        assert_eq!(gpu_display_name(Vendor::Amd, Some(&model)), "AMD Radeon RX 7700 XT / 7800 XT");
        assert_eq!(gpu_display_name(Vendor::Amd, ids.model(0x1002, 0x164e).as_deref()), "AMD Raphael");
        assert_eq!(gpu_display_name(Vendor::Nvidia, ids.model(0x10de, 0x2786).as_deref()), "NVIDIA GeForce RTX 4070");
        // Unknown device or no database: still a useful name.
        assert_eq!(ids.model(0x1002, 0xffff), None);
        assert_eq!(gpu_display_name(Vendor::Intel, None), "Intel GPU");
    }

    #[test]
    fn single_amd_card_on_mesa() {
        // The usual Arch setup: FFmpeg lists AMF too, but AMD's runtime isn't installed.
        let caps = caps_with(&["h264_amf", "hevc_amf", "h264_vaapi", "hevc_vaapi", "av1_vaapi"]);
        let devices = [device(0, Vendor::Amd, "AMD Radeon RX 7800 XT")];
        let found = detect_gpu_encoders_with(&devices, &caps, |g| g.codec.backend == Backend::Vaapi);
        assert_eq!(found.len(), 3);
        assert!(found.iter().all(|g| g.codec.vendor == Vendor::Amd && g.tuned && g.name == "AMD Radeon RX 7800 XT"));
    }

    #[test]
    fn intel_igpu_no_longer_hides_amd_card() {
        // Quick Sync covers every codec on the Intel iGPU; before, that hid the AMD card.
        let caps = caps_with(&["h264_qsv", "hevc_qsv", "av1_qsv", "h264_vaapi", "hevc_vaapi", "av1_vaapi"]);
        let devices = [device(0, Vendor::Intel, "Intel UHD Graphics 770"), device(1, Vendor::Amd, "AMD Radeon RX 6700 XT")];
        let found = detect_gpu_encoders_with(&devices, &caps, |_| true);
        let engines: Vec<(Backend, Vendor)> = found.iter().map(|g| (g.codec.backend, g.codec.vendor)).collect();
        assert!(engines.contains(&(Backend::Qsv, Vendor::Intel)));
        assert!(engines.contains(&(Backend::Vaapi, Vendor::Amd)));
        // VA-API on the Intel iGPU would duplicate Quick Sync, so it's skipped.
        assert!(!engines.contains(&(Backend::Vaapi, Vendor::Intel)), "{:?}", names(&found));
    }

    #[test]
    fn two_amd_gpus_are_both_listed_and_told_apart() {
        let mut caps = caps_with(ALL_VAAPI);
        let devices = [device(0, Vendor::Amd, "AMD Raphael"), device(1, Vendor::Amd, "AMD Radeon RX 7800 XT")];
        caps.gpu = detect_gpu_encoders_with(&devices, &caps, |_| true);
        caps.hardware_checked = true;
        let engines = caps.engines();
        let labels: Vec<&str> = engines.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(labels, ["CPU", "AMD Raphael", "AMD Radeon RX 7800 XT"]);
        let discrete = engines[2].0;
        assert_eq!(discrete.slot, 1);
        assert_eq!(caps.codecs_on(discrete).len(), 3);
        assert!(caps.codecs_on(discrete).iter().all(|c| c.engine() == discrete));
    }

    #[test]
    fn same_model_twice_gets_render_node_in_name() {
        let mut caps = caps_with(&["hevc_vaapi"]);
        let devices = [device(0, Vendor::Amd, "AMD Radeon RX 580"), device(1, Vendor::Amd, "AMD Radeon RX 580")];
        caps.gpu = detect_gpu_encoders_with(&devices, &caps, |_| true);
        let labels: Vec<String> = caps.engines().into_iter().map(|(_, n)| n).collect();
        assert_eq!(labels[1], "AMD Radeon RX 580 (VA-API, renderD128)");
        assert_eq!(labels[2], "AMD Radeon RX 580 (VA-API, renderD129)");
    }

    #[test]
    fn rejected_quality_settings_fall_back_to_plain() {
        let caps = caps_with(&["hevc_nvenc"]);
        let devices = [device(0, Vendor::Nvidia, "NVIDIA GeForce GTX 1080")];
        // A Pascal card: B-frame references aren't supported, so the tuned test fails.
        let found = detect_gpu_encoders_with(&devices, &caps, |g| !g.tuned);
        assert_eq!(found.len(), 1);
        assert!(!found[0].tuned);
        assert_eq!(found[0].name, "NVIDIA GeForce GTX 1080");
        // A GPU that fails both ways isn't listed at all.
        assert!(detect_gpu_encoders_with(&devices, &caps, |_| false).is_empty());
    }

    #[test]
    fn codec_saved_by_older_version_still_matches() {
        // Settings saved before codecs recorded their vendor.
        let old: Codec = serde_json::from_str(r#"{"family":"H265","backend":"Nvenc"}"#).unwrap();
        let mut caps = software_only();
        caps.hardware_checked = true;
        caps.gpu.push(GpuEncoder {
            codec: NVENC_HEVC,
            encoder: "hevc_nvenc".into(),
            device: None,
            name: "NVIDIA GeForce RTX 4070".into(),
            tuned: true,
        });
        assert_eq!(caps.resolve_codec(old), (NVENC_HEVC, false));
    }
}
