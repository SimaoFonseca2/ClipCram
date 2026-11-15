//! `clipcram encode`: encodes without opening a window, for scripts, file-manager menus and
//! testing. Every edit the window offers is available as an option.

use crate::config;
use crate::edit::{Crop, Edit, Range, TextPos, TextSize};
use crate::encode::{
    self, AudioMode, Codec, Denoise, EncodeSettings, Family, FpsMode, JobSpec, OutputFormat, ResolutionMode,
    SizePreset, Speed, TrackMode, WorkerMsg,
};
use crate::ffmpeg::{self, Caps, Tools};
use crate::util;
use std::io::Write;
use std::path::PathBuf;

pub const USAGE: &str = "\
Usage: clipcram encode [options] VIDEO...

Squeezes each video under a size limit without opening the window.

Size:
  --size MB              Size limit in MiB (default: the app's setting, else 9.5)
  --preset NAME          discord (9.5 MiB), nitro-basic, nitro, email
  --saved                Start from the settings saved by the app
Encoding:
  --codec NAME           h265, h264 or av1
  --on WHERE             cpu, gpu, or part of a GPU's name (\"7800\", \"nvidia\")
  --quality NAME         faster, balanced or best
  --resolution R         auto, original, 2160, 1440, 1080, 720, 540, 480, 360
  --fps F                auto, original, 60 or 30
  --audio A              64, 96, 128, 160 or off
  --tracks T             mix, first or all
  --keep-hdr             Don't convert HDR to SDR
  --denoise D            off, light or strong
  --normalize            Even out loudness
  --format F             mp4, gif or webp
  --no-copy              Always re-encode, even when the clip already fits
  --no-quality           Skip the quality check after encoding
Editing (times in seconds or m:ss):
  --start T  --end T     Trim
  --cut A-B              Remove a part (repeatable)
  --mute A-B             Silence a part (repeatable)
  --crop W:H:X:Y         Crop in pixels, or an aspect ratio: 16:9, 9:16, 1:1, 4:3
  --playback-speed X     0.25 to 4
  --text TEXT            Caption  (--text-pos top|bottom, --text-size small|medium|large)
  --track-volume N=V     Volume of audio track N (1-based) when mixing, e.g. 2=0.5
Output:
  --out DIR              Output folder (default: the app's setting)
  --name NAME            Output file name (one video only)
  --json                 Print one JSON line per video
  --notify               Show a desktop notification when done
";

/// A change the command line makes to the encoding settings.
type SettingChange = Box<dyn Fn(&mut EncodeSettings)>;

struct Options {
    saved: bool,
    size_mib: Option<f64>,
    settings: Vec<SettingChange>,
    on: Option<String>,
    copy: bool,
    quality: bool,
    start: Option<f64>,
    end: Option<f64>,
    edit: Edit,
    crop: Option<String>,
    out: Option<PathBuf>,
    name: Option<String>,
    json: bool,
    notify: bool,
    files: Vec<PathBuf>,
}

fn time(v: &str) -> Result<f64, String> {
    util::parse_time(v).ok_or_else(|| format!("Not a time: {v}"))
}

fn range(v: &str) -> Result<Range, String> {
    let (a, b) = v.split_once('-').ok_or_else(|| format!("Expected START-END, got {v}"))?;
    Ok(Range::new(time(a)?, time(b)?))
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut o = Options {
        saved: false,
        size_mib: None,
        settings: Vec::new(),
        on: None,
        copy: true,
        quality: true,
        start: None,
        end: None,
        edit: Edit::default(),
        crop: None,
        out: None,
        name: None,
        json: false,
        notify: false,
        files: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--saved" => o.saved = true,
            "--size" => o.size_mib = Some(value()?.parse().map_err(|_| "Bad --size".to_string())?),
            "--preset" => {
                let p = match value()?.as_str() {
                    "discord" => SizePreset::Discord10,
                    "nitro-basic" => SizePreset::Nitro50,
                    "nitro" => SizePreset::Nitro500,
                    "email" => SizePreset::Email,
                    v => return Err(format!("Unknown preset {v}")),
                };
                o.size_mib = Some(p.target_mib(9.5));
            }
            "--codec" => {
                let family = match value()?.as_str() {
                    "h265" | "hevc" => Family::H265,
                    "h264" | "avc" => Family::H264,
                    "av1" => Family::Av1,
                    v => return Err(format!("Unknown codec {v}")),
                };
                o.settings.push(Box::new(move |s| s.codec.family = family));
            }
            "--on" => o.on = Some(value()?.to_lowercase()),
            "--quality" => {
                let speed = match value()?.as_str() {
                    "faster" => Speed::Faster,
                    "balanced" => Speed::Balanced,
                    "best" => Speed::Best,
                    v => return Err(format!("Unknown quality {v}")),
                };
                o.settings.push(Box::new(move |s| s.speed = speed));
            }
            "--resolution" => {
                let r = match value()?.as_str() {
                    "auto" => ResolutionMode::Auto,
                    "original" => ResolutionMode::Original,
                    v => ResolutionMode::Short(v.trim_end_matches('p').parse().map_err(|_| format!("Bad resolution {v}"))?),
                };
                o.settings.push(Box::new(move |s| s.resolution = r));
            }
            "--fps" => {
                let f = match value()?.as_str() {
                    "auto" => FpsMode::Auto,
                    "original" => FpsMode::Original,
                    "60" => FpsMode::Max60,
                    "30" => FpsMode::Max30,
                    v => return Err(format!("Unknown fps {v}")),
                };
                o.settings.push(Box::new(move |s| s.fps = f));
            }
            "--audio" => {
                let a = match value()?.as_str() {
                    "64" => AudioMode::Aac64,
                    "96" => AudioMode::Aac96,
                    "128" => AudioMode::Aac128,
                    "160" => AudioMode::Aac160,
                    "off" => AudioMode::Off,
                    v => return Err(format!("Unknown audio {v}")),
                };
                o.settings.push(Box::new(move |s| s.audio = a));
            }
            "--tracks" => {
                let t = match value()?.as_str() {
                    "mix" => TrackMode::Mix,
                    "first" => TrackMode::First,
                    "all" => TrackMode::KeepAll,
                    v => return Err(format!("Unknown tracks {v}")),
                };
                o.settings.push(Box::new(move |s| s.tracks = t));
            }
            "--keep-hdr" => o.settings.push(Box::new(|s| s.hdr_to_sdr = false)),
            "--denoise" => {
                let d = match value()?.as_str() {
                    "off" => Denoise::Off,
                    "light" => Denoise::Light,
                    "strong" => Denoise::Strong,
                    v => return Err(format!("Unknown denoise {v}")),
                };
                o.settings.push(Box::new(move |s| s.denoise = d));
            }
            "--normalize" => o.settings.push(Box::new(|s| s.normalize = true)),
            "--format" => {
                let f = match value()?.as_str() {
                    "mp4" => OutputFormat::Mp4,
                    "gif" => OutputFormat::Gif,
                    "webp" => OutputFormat::Webp,
                    v => return Err(format!("Unknown format {v}")),
                };
                o.settings.push(Box::new(move |s| s.format = f));
            }
            "--no-copy" => o.copy = false,
            "--no-quality" => o.quality = false,
            "--start" => o.start = Some(time(&value()?)?),
            "--end" => o.end = Some(time(&value()?)?),
            "--cut" => o.edit.add_cut(range(&value()?)?),
            "--mute" => o.edit.add_mute(range(&value()?)?),
            "--crop" => o.crop = Some(value()?),
            "--playback-speed" => o.edit.speed = value()?.parse().map_err(|_| "Bad --playback-speed".to_string())?,
            "--text" => o.edit.text = value()?,
            "--text-pos" => {
                o.edit.text_pos = match value()?.as_str() {
                    "top" => TextPos::Top,
                    "bottom" => TextPos::Bottom,
                    v => return Err(format!("Unknown text position {v}")),
                }
            }
            "--text-size" => {
                o.edit.text_size = match value()?.as_str() {
                    "small" => TextSize::Small,
                    "medium" => TextSize::Medium,
                    "large" => TextSize::Large,
                    v => return Err(format!("Unknown text size {v}")),
                }
            }
            "--track-volume" => {
                let v = value()?;
                let (n, vol) = v.split_once('=').ok_or("Expected --track-volume N=V")?;
                let n: usize = n.parse().map_err(|_| "Bad track number")?;
                let vol: f32 = vol.parse().map_err(|_| "Bad volume")?;
                if n == 0 {
                    return Err("Track numbers start at 1".to_string());
                }
                if o.edit.track_volumes.len() < n {
                    o.edit.track_volumes.resize(n, 1.0);
                }
                o.edit.track_volumes[n - 1] = vol;
            }
            "--out" => o.out = Some(PathBuf::from(value()?)),
            "--name" => o.name = Some(value()?),
            "--json" => o.json = true,
            "--notify" => o.notify = true,
            "-h" | "--help" => return Err(String::new()),
            v if v.starts_with("--") => return Err(format!("Unknown option {v}")),
            _ => o.files.push(PathBuf::from(arg)),
        }
    }
    if o.files.is_empty() {
        return Err("No videos given.".to_string());
    }
    if o.name.is_some() && o.files.len() > 1 {
        return Err("--name only works with one video.".to_string());
    }
    Ok(o)
}

fn parse_crop(v: &str, w: u32, h: u32) -> Result<Crop, String> {
    let parts: Vec<u32> = v.split(':').map(|p| p.parse().map_err(|_| format!("Bad crop {v}"))).collect::<Result<_, _>>()?;
    match parts[..] {
        [aw, ah] => Ok(Crop::centered(w, h, aw, ah)),
        [cw, ch, x, y] => Ok(Crop { x, y, w: cw, h: ch }.clamped(w, h)),
        _ => Err(format!("Bad crop {v}: use W:H:X:Y or an aspect ratio like 9:16")),
    }
}

/// Picks the codec: the requested family on the requested CPU or GPU.
fn choose_codec(caps: &Caps, family: Family, on: Option<&str>) -> Result<Codec, String> {
    let engines = caps.engines();
    let engine = match on {
        None => return Ok(caps.resolve_codec(Codec { family, ..Codec::H265 }).0),
        Some("cpu") => encode::Engine::CPU,
        Some("gpu") => engines.iter().find(|(e, _)| e.backend != encode::Backend::Software).map(|(e, _)| *e).ok_or("No working GPU encoder found.")?,
        Some(name) => engines
            .iter()
            .find(|(_, n)| n.to_lowercase().contains(name))
            .map(|(e, _)| *e)
            .ok_or_else(|| format!("No encoder matches \"{name}\". Found: {}", engines.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(", ")))?,
    };
    let codecs = caps.codecs_on(engine);
    codecs
        .iter()
        .find(|c| c.family == family)
        .or(codecs.first())
        .copied()
        .ok_or_else(|| "That encoder can't encode any supported codec.".to_string())
}

/// Runs the command line. Returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    let o = match parse(args) {
        Ok(o) => o,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("clipcram: {e}\n");
            }
            eprint!("{USAGE}");
            return if e.is_empty() { 0 } else { 2 };
        }
    };
    let tools = match Tools::find() {
        Ok(t) => t,
        Err(missing) => {
            eprintln!("clipcram: missing required programs: {}", missing.join(", "));
            return 1;
        }
    };
    let (persisted, saved_settings) = if o.saved { config::load_from_disk() } else { Default::default() };
    let mut caps = Caps::detect(&tools);
    let wants_gpu = o.on.as_deref().is_some_and(|w| w != "cpu") || (o.saved && saved_settings.codec.is_gpu());
    if wants_gpu {
        eprintln!("Checking GPU encoders…");
        let (gpu, hwaccel) = ffmpeg::detect_hardware(&tools, &caps);
        caps.gpu = gpu;
        caps.hwaccel = hwaccel;
        caps.hardware_checked = true;
    }
    let mut settings = if o.saved { saved_settings } else { EncodeSettings::default() };
    for apply in &o.settings {
        apply(&mut settings);
    }
    if o.on.is_some() || !caps.codec_options().contains(&settings.codec.normalized()) {
        match choose_codec(&caps, settings.codec.family, o.on.as_deref()) {
            Ok(c) => settings.codec = c,
            Err(e) => {
                eprintln!("clipcram: {e}");
                return 1;
            }
        }
    }
    let target_mib = o.size_mib.unwrap_or(if o.saved { persisted.target_mib() } else { 9.5 });
    let out_dir = o.out.clone().unwrap_or_else(|| util::expand_tilde(&persisted.default_output_dir));

    let mut failed = 0;
    let mut made: Vec<String> = Vec::new();
    for file in &o.files {
        let info = match ffmpeg::probe(&tools, file) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("clipcram: {}: {e}", file.display());
                failed += 1;
                continue;
            }
        };
        let mut edit = o.edit.clone();
        if let Some(c) = &o.crop {
            match parse_crop(c, info.width, info.height) {
                Ok(c) => edit.crop = Some(c),
                Err(e) => {
                    eprintln!("clipcram: {e}");
                    return 2;
                }
            }
        }
        let name = o.name.clone().unwrap_or_else(|| encode::output_name_for(file, &settings));
        let spec = JobSpec {
            item_id: 0,
            input: file.clone(),
            output_dir: out_dir.clone(),
            output_name: encode::with_format_extension(&name, settings.format),
            start: o.start.unwrap_or(0.0).clamp(0.0, info.duration),
            end: o.end.unwrap_or(info.duration).min(info.duration),
            settings,
            info,
            target_bytes: (target_mib * 1024.0 * 1024.0) as u64,
            edit,
            copy_if_fits: o.copy,
            measure_quality: o.quality,
        };
        eprintln!("{} → {} on {}", file.display(), settings.codec.family_label(), caps.engine_name(settings.codec.engine()));
        let send = |m: WorkerMsg| match m {
            WorkerMsg::Status(s) => eprintln!("  {s}"),
            WorkerMsg::Progress { fraction, .. } => {
                eprint!("\r  {:>3.0}%", fraction * 100.0);
                let _ = std::io::stderr().flush();
            }
            _ => {}
        };
        match encode::run_blocking(&tools, &caps, &spec, &send) {
            WorkerMsg::Done { path, size, quality, copied } => {
                eprintln!();
                let q = quality.map(|q| format!(", quality {} (SSIM {q:.4})", encode::quality_label(q))).unwrap_or_default();
                if o.json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "input": file, "output": path, "bytes": size, "ssim": quality, "copied": copied,
                        })
                    );
                } else {
                    println!("{} ({:.2} MiB{}{q})", path.display(), util::mib(size), if copied { ", copied without re-encoding" } else { "" });
                }
                made.push(format!("{} ({:.1} MiB)", util::file_name_lossy(&path), util::mib(size)));
            }
            WorkerMsg::Failed(e) => {
                eprintln!("\nclipcram: {}: {e}", file.display());
                failed += 1;
            }
            _ => failed += 1,
        }
    }
    if o.notify {
        let body = if failed > 0 {
            format!("{} done, {failed} failed.", made.len())
        } else {
            made.join("\n")
        };
        crate::services::notify("ClipCram", &body);
        // Give the notification helper a moment before the process exits.
        std::thread::sleep(std::time::Duration::from_millis(800));
    }
    if failed > 0 { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_edits() {
        let o = parse(&args("--cut 1-2 --cut 1:30-1:40 --mute 3-4 --playback-speed 2 --track-volume 2=0.5 a.mp4")).unwrap();
        assert_eq!(o.edit.cuts, vec![Range::new(1.0, 2.0), Range::new(90.0, 100.0)]);
        assert_eq!(o.edit.mutes.len(), 1);
        assert_eq!(o.edit.speed, 2.0);
        assert_eq!(o.edit.track_volumes, vec![1.0, 0.5]);
        assert_eq!(o.files, vec![PathBuf::from("a.mp4")]);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse(&args("--codec vp9 a.mp4")).is_err());
        assert!(parse(&args("--track-volume 0=1 a.mp4")).is_err());
        assert!(parse(&args("--name x a.mp4 b.mp4")).is_err());
        assert!(parse(&args("--size")).is_err());
        assert!(parse(&args("")).is_err());
        assert_eq!(parse_crop("9:16", 1920, 1080).unwrap().w, 606);
        assert_eq!(parse_crop("100:100:10:10", 1920, 1080).unwrap(), Crop { x: 10, y: 10, w: 100, h: 100 });
        assert!(parse_crop("1:2:3", 1920, 1080).is_err());
    }
}
