//! Helpers the window uses that don't draw anything: timeline thumbnails and waveforms,
//! the watch folder, the clipboard, desktop notifications, Discord webhooks, update checks,
//! the GPU detection cache, and "Squeeze with ClipCram" entries in file-manager menus.
//!
//! Network access goes through the system's `curl`, which ships with Windows 10 and later,
//! macOS, and practically every Linux system (on Arch, pacman itself depends on it).

use crate::ffmpeg::{self, GpuEncoder, MediaInfo, Tools};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;

// ============================================================
// THUMBNAILS AND WAVEFORM
// ============================================================

pub const THUMB_HEIGHT: usize = 48;
pub const THUMB_COUNT: usize = 12;
pub const WAVE_BUCKETS: usize = 1200;
const WAVE_RATE: usize = 4000;

pub struct Thumb {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub enum MediaMsg {
    Thumbs { path: PathBuf, thumbs: Vec<Thumb> },
    Waveform { path: PathBuf, peaks: Vec<f32> },
}

/// Loads timeline thumbnails and the audio waveform for a video in the background.
pub fn spawn_media(tools: Tools, path: PathBuf, info: &MediaInfo, tx: Sender<MediaMsg>, wake: impl Fn() + Send + Sync + 'static) {
    let info = info.clone();
    let wake = std::sync::Arc::new(wake);
    {
        let (tools, path, tx, wake, info) = (tools.clone(), path.clone(), tx.clone(), wake.clone(), info.clone());
        thread::spawn(move || {
            let thumbs = load_thumbs(&tools, &path, &info);
            let _ = tx.send(MediaMsg::Thumbs { path, thumbs });
            wake();
        });
    }
    if info.audio_tracks > 0 {
        thread::spawn(move || {
            let peaks = load_waveform(&tools, &path, &info);
            let _ = tx.send(MediaMsg::Waveform { path, peaks });
            wake();
        });
    }
}

fn load_thumbs(tools: &Tools, path: &Path, info: &MediaInfo) -> Vec<Thumb> {
    let h = THUMB_HEIGHT;
    let w = ((h as f64 * info.width as f64 / info.height.max(1) as f64 / 2.0).round() as usize * 2).clamp(16, 200);
    let mut out = Vec::new();
    for i in 0..THUMB_COUNT {
        let t = info.duration * (i as f64 + 0.5) / THUMB_COUNT as f64;
        let output = ffmpeg::command(&tools.ffmpeg)
            .args(["-hide_banner", "-nostdin", "-v", "error", "-ss", &format!("{t:.3}"), "-i"])
            .arg(path)
            .args(["-map", &format!("0:{}", info.video_index), "-frames:v", "1", "-an", "-sn"])
            .args(["-vf", &format!("scale={w}:{h}:flags=fast_bilinear"), "-pix_fmt", "rgba", "-f", "rawvideo", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        match output {
            Ok(o) if o.stdout.len() == w * h * 4 => out.push(Thumb { width: w, height: h, rgba: o.stdout }),
            _ => out.push(Thumb { width: w, height: h, rgba: vec![0; w * h * 4] }),
        }
    }
    out
}

fn load_waveform(tools: &Tools, path: &Path, info: &MediaInfo) -> Vec<f32> {
    let mut cmd = ffmpeg::command(&tools.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-v", "error", "-i"]).arg(path).arg("-vn");
    if info.audio_tracks > 1 {
        let inputs: String = (0..info.audio_tracks).map(|i| format!("[0:a:{i}]")).collect();
        cmd.args(["-filter_complex", &format!("{inputs}amix=inputs={}:normalize=0[a]", info.audio_tracks), "-map", "[a]"]);
    } else {
        cmd.args(["-map", "0:a:0"]);
    }
    cmd.args(["-ac", "1", "-ar", &WAVE_RATE.to_string(), "-f", "s16le", "-"]);
    let Ok(mut child) = cmd.stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { return Vec::new() };
    let mut bytes = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_end(&mut bytes);
    }
    let _ = child.wait();
    let total = (info.duration * WAVE_RATE as f64) as usize;
    let samples = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]));
    peaks(samples, total, WAVE_BUCKETS)
}

/// Peak level per bucket, scaled so the loudest bucket is 1.0. `total` is the expected
/// sample count, so buckets line up with the timeline even if decoding stops short.
pub fn peaks(samples: impl Iterator<Item = i16>, total: usize, buckets: usize) -> Vec<f32> {
    let mut out = vec![0f32; buckets];
    let total = total.max(1);
    for (i, s) in samples.enumerate() {
        let b = (i * buckets / total).min(buckets - 1);
        let v = (s as f32).abs() / 32768.0;
        if v > out[b] {
            out[b] = v;
        }
    }
    let max = out.iter().cloned().fold(0.0f32, f32::max);
    if max > 0.0 {
        for v in &mut out {
            *v /= max;
        }
    }
    out
}

// ============================================================
// FRAMES FOR THE BEFORE/AFTER COMPARISON
// ============================================================

/// One RGBA frame of `path` at `t` seconds, run through `vf` (which must end at exactly
/// `w`x`h`). Accurate seeking, so both sides of the comparison show the same moment.
pub fn grab_frame(tools: &Tools, path: &Path, t: f64, vf: &str, w: usize, h: usize) -> Option<Vec<u8>> {
    let output = ffmpeg::command(&tools.ffmpeg)
        .args(["-hide_banner", "-nostdin", "-v", "error", "-ss", &format!("{:.3}", t.max(0.0)), "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-frames:v", "1", "-an", "-sn", "-vf", vf, "-pix_fmt", "rgba", "-f", "rawvideo", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    (output.stdout.len() == w * h * 4).then_some(output.stdout)
}

/// Size to compare at: the (cropped) source size, at most `max_long` pixels on the long
/// side, with even dimensions.
pub fn compare_size(w: u32, h: u32, max_long: u32) -> (usize, usize) {
    let scale = (max_long as f64 / w.max(h).max(1) as f64).min(1.0);
    let even = |v: f64| ((v / 2.0).round() as usize * 2).max(2);
    (even(w as f64 * scale), even(h as f64 * scale))
}

// ============================================================
// WATCH FOLDER
// ============================================================

/// Notices new videos in a folder, such as OBS's replay buffer saves. A file counts once
/// its size has stopped changing between two checks, so half-written files are skipped.
pub struct Watcher {
    pub dir: PathBuf,
    seen: HashSet<PathBuf>,
    sizes: HashMap<PathBuf, u64>,
}

impl Watcher {
    /// Files already in the folder are ignored; only new ones are reported.
    pub fn new(dir: PathBuf, extensions: &[&str]) -> Watcher {
        let seen = list_videos(&dir, extensions).into_iter().collect();
        Watcher { dir, seen, sizes: HashMap::new() }
    }

    pub fn poll(&mut self, extensions: &[&str]) -> Vec<PathBuf> {
        let mut ready = Vec::new();
        for path in list_videos(&self.dir, extensions) {
            if self.seen.contains(&path) {
                continue;
            }
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if size > 0 && self.sizes.get(&path) == Some(&size) {
                self.sizes.remove(&path);
                self.seen.insert(path.clone());
                ready.push(path);
            } else {
                self.sizes.insert(path, size);
            }
        }
        ready.sort();
        ready
    }
}

fn list_videos(dir: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .filter(|p| {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    !name.starts_with('.')
                        && p.extension().is_some_and(|e| extensions.contains(&e.to_string_lossy().to_lowercase().as_str()))
                })
                .collect()
        })
        .unwrap_or_default()
}

// ============================================================
// CLIPBOARD AND NOTIFICATIONS
// ============================================================

/// Puts files on the clipboard the way a file manager's Copy does, so they can be pasted
/// into Discord. The clipboard object is kept by the caller: on Linux the copied data is
/// served from it for as long as it lives.
pub fn copy_files(clipboard: &mut Option<arboard::Clipboard>, paths: &[PathBuf]) -> Result<(), String> {
    if clipboard.is_none() {
        *clipboard = Some(arboard::Clipboard::new().map_err(|e| e.to_string())?);
    }
    let cb = clipboard.as_mut().ok_or("No clipboard")?;
    cb.set().file_list(paths).map_err(|e| e.to_string())
}

/// A desktop notification, best effort: `notify-send` on Linux, Notification Center on
/// macOS, a toast through PowerShell on Windows.
pub fn notify(title: &str, body: &str) {
    let (title, body) = (title.to_string(), body.to_string());
    thread::spawn(move || {
        let mut cmd = if cfg!(target_os = "macos") {
            let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
            let mut c = Command::new("osascript");
            c.arg("-e").arg(format!("display notification \"{}\" with title \"{}\"", esc(&body), esc(&title)));
            c
        } else if cfg!(windows) {
            let esc = |s: &str| s.replace('\'', "''").replace('<', "&lt;").replace('>', "&gt;").replace('&', "&amp;");
            let script = format!(
                "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null; \
                 $x = New-Object Windows.Data.Xml.Dom.XmlDocument; \
                 $x.LoadXml('<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>'); \
                 [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe').Show([Windows.UI.Notifications.ToastNotification]::new($x))",
                esc(&title),
                esc(&body)
            );
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script]);
            hide_console(&mut c);
            c
        } else {
            let mut c = Command::new("notify-send");
            c.args(["--app-name=ClipCram", "--icon=clipcram", &title, &body]);
            c
        };
        let _ = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status();
    });
}

fn hide_console(_cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        _cmd.creation_flags(0x0800_0000);
    }
}

fn curl() -> Command {
    let mut c = Command::new("curl");
    hide_console(&mut c);
    c
}

/// Runs curl and returns (HTTP status, response body).
fn curl_request(args: &[String]) -> Result<(u16, String), String> {
    let out = curl()
        .args(args)
        .args(["-sS", "-w", "\n%{http_code}"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Could not run curl ({e}). Install curl to use this."))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", text.as_str()));
    let code: u16 = code.trim().parse().unwrap_or(0);
    if code == 0 {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() { "The request failed.".to_string() } else { err });
    }
    Ok((code, body.to_string()))
}

// ============================================================
// DISCORD WEBHOOK
// ============================================================

/// Whether a URL looks like a webhook this is allowed to post to: an HTTPS address, or a
/// local one for testing.
pub fn webhook_url_ok(url: &str) -> bool {
    let url = url.trim();
    url.starts_with("https://") || url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost")
}

/// Posts a file to a Discord webhook, with an optional message.
pub fn post_to_webhook(url: &str, file: &Path, message: &str) -> Result<(), String> {
    if !webhook_url_ok(url) {
        return Err("The webhook address must start with https://".to_string());
    }
    let payload = serde_json::json!({ "content": message, "allowed_mentions": { "parse": [] } }).to_string();
    let path = file.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let args = vec![
        "--max-time".to_string(),
        "900".to_string(),
        "-F".to_string(),
        format!("payload_json={payload}"),
        "-F".to_string(),
        format!("files[0]=@\"{path}\""),
        url.trim().to_string(),
    ];
    let (code, body) = curl_request(&args)?;
    if (200..300).contains(&code) {
        return Ok(());
    }
    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["message"].as_str().map(String::from))
        .unwrap_or(body);
    Err(match code {
        413 => format!("Discord rejected the file as too large for this server ({detail})."),
        401 | 404 => "Discord didn't accept the webhook address. Check that it was copied whole.".to_string(),
        _ => format!("Discord answered {code}: {detail}"),
    })
}

// ============================================================
// UPDATE CHECK
// ============================================================

/// The newest release tag on GitHub if it's newer than this version, None if not (or if
/// there are no releases).
pub fn newer_release(repo_url: &str, current: &str) -> Result<Option<String>, String> {
    let repo = repo_url.trim_end_matches('/').trim_end_matches(".git");
    let path = repo.strip_prefix("https://github.com/").ok_or("Not a GitHub repository")?;
    let args = vec![
        "--max-time".to_string(),
        "10".to_string(),
        "-H".to_string(),
        "Accept: application/vnd.github+json".to_string(),
        "-H".to_string(),
        format!("User-Agent: ClipCram/{current}"),
        format!("https://api.github.com/repos/{path}/releases/latest"),
    ];
    let (code, body) = curl_request(&args)?;
    match code {
        404 => Ok(None),
        200 => Ok(release_tag(&body).filter(|tag| is_newer(tag, current))),
        // 403 is GitHub's limit of 60 anonymous requests an hour per address.
        403 => Err("GitHub's rate limit was reached; try again later".to_string()),
        c => Err(format!("GitHub answered {c}")),
    }
}

/// The tag of a release from GitHub's "latest release" JSON, skipping drafts and prereleases.
pub fn release_tag(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        return None;
    }
    v["tag_name"].as_str().filter(|t| !t.is_empty()).map(String::from)
}

/// "v1.2.0" is newer than "1.1.9"; anything unparseable isn't newer.
pub fn is_newer(tag: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> {
        v.trim().trim_start_matches(['v', 'V']).split(['-', '+']).next()?.split('.').map(|p| p.parse().ok()).collect()
    };
    match (parse(tag), parse(current)) {
        (Some(mut a), Some(mut b)) => {
            let n = a.len().max(b.len());
            a.resize(n, 0);
            b.resize(n, 0);
            a > b
        }
        _ => false,
    }
}

// ============================================================
// GPU DETECTION CACHE
// ============================================================

#[derive(Serialize, Deserialize)]
struct GpuCache {
    key: String,
    gpu: Vec<GpuEncoder>,
    hwaccel: Option<String>,
}

/// What GPU detection results depend on: the FFmpeg build, the GPUs, the kernel and the
/// NVIDIA driver. When any of them changes, the cache is ignored.
pub fn gpu_fingerprint(tools: &Tools) -> String {
    let mut parts = vec![tools.ffmpeg.display().to_string()];
    if let Ok(m) = std::fs::metadata(&tools.ffmpeg) {
        parts.push(m.len().to_string());
        if let Ok(t) = m.modified().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).map_err(std::io::Error::other)) {
            parts.push(t.as_secs().to_string());
        }
    }
    if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        let mut nodes: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("renderD"))
            .collect();
        nodes.sort();
        for n in nodes {
            for f in ["vendor", "device"] {
                parts.push(std::fs::read_to_string(format!("/sys/class/drm/{n}/device/{f}")).unwrap_or_default().trim().to_string());
            }
        }
    }
    for f in ["/proc/sys/kernel/osrelease", "/proc/driver/nvidia/version"] {
        parts.push(std::fs::read_to_string(f).unwrap_or_default().lines().next().unwrap_or("").to_string());
    }
    parts.join("|")
}

fn cache_file() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("clipcram").join("gpu.json"))
}

pub fn load_gpu_cache(key: &str) -> Option<(Vec<GpuEncoder>, Option<String>)> {
    let text = std::fs::read_to_string(cache_file()?).ok()?;
    let cache: GpuCache = serde_json::from_str(&text).ok()?;
    (cache.key == key).then_some((cache.gpu, cache.hwaccel))
}

pub fn save_gpu_cache(key: &str, gpu: &[GpuEncoder], hwaccel: &Option<String>) {
    let Some(file) = cache_file() else { return };
    let cache = GpuCache { key: key.to_string(), gpu: gpu.to_vec(), hwaccel: hwaccel.clone() };
    if let (Some(dir), Ok(text)) = (file.parent(), serde_json::to_string(&cache)) {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(&file, text);
    }
}

// ============================================================
// FILE-MANAGER MENU
// ============================================================

/// File types the menu entry appears on (Windows; Linux uses MIME types).
#[cfg_attr(not(windows), allow(dead_code))]
pub const MENU_EXTENSIONS: &[&str] = &["mp4", "mkv", "mov", "avi", "webm", "m4v", "ts", "flv", "wmv"];

fn exe() -> Result<String, String> {
    std::env::current_exe().map(|p| p.display().to_string()).map_err(|e| e.to_string())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn linux_menu_files() -> Vec<PathBuf> {
    let Some(data) = dirs::data_dir() else { return Vec::new() };
    vec![
        data.join("kio/servicemenus/clipcram.desktop"),
        data.join("kservices5/ServiceMenus/clipcram.desktop"),
        data.join("nautilus/scripts/Squeeze with ClipCram"),
        data.join("nemo/actions/clipcram.nemo_action"),
    ]
}

/// Adds "Squeeze with ClipCram" (encode with the saved settings, in the background) to the
/// right-click menu of video files. On Linux this covers Dolphin (KDE), Nautilus (GNOME)
/// and Nemo (Cinnamon); on Windows, Explorer.
pub fn install_menu() -> Result<String, String> {
    let exe = exe()?;
    #[cfg(windows)]
    {
        for ext in MENU_EXTENSIONS {
            let key = format!("HKCU\\Software\\Classes\\SystemFileAssociations\\.{ext}\\shell\\ClipCram");
            let command = format!("\"{exe}\" encode --saved --notify \"%1\"");
            for (k, v) in [(key.clone(), "Squeeze with ClipCram".to_string()), (format!("{key}\\command"), command)] {
                let ok = Command::new("reg").args(["add", &k, "/ve", "/d", &v, "/f"]).stdout(Stdio::null()).status();
                if !ok.map(|s| s.success()).unwrap_or(false) {
                    return Err(format!("Could not write the registry key {k}"));
                }
            }
            let _ = Command::new("reg").args(["add", &key, "/v", "Icon", "/d", &exe, "/f"]).stdout(Stdio::null()).status();
        }
        return Ok("Added \"Squeeze with ClipCram\" to Explorer's right-click menu for video files.".to_string());
    }
    #[cfg(target_os = "macos")]
    {
        let _ = exe;
        return Err("On macOS, use the command line instead: clipcram encode --saved FILE".to_string());
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let quoted = format!("\"{}\"", exe.replace('"', "\\\""));
        let mimes = "video/mp4;video/x-matroska;video/quicktime;video/x-msvideo;video/webm;video/x-m4v;video/mp2t;video/x-flv;video/x-ms-wmv;";
        let kde = format!(
            "[Desktop Entry]\nType=Service\nMimeType={mimes}\nActions=squeeze;open\nX-KDE-ServiceTypes=KonqPopupMenu/Plugin\nX-KDE-Submenu=ClipCram\n\n\
             [Desktop Action squeeze]\nName=Squeeze with saved settings\nIcon=clipcram\nExec={quoted} encode --saved --notify %F\n\n\
             [Desktop Action open]\nName=Open in ClipCram\nIcon=clipcram\nExec={quoted} %F\n"
        );
        let script = format!(
            "#!/bin/sh\n# Added by ClipCram: squeezes the selected videos with ClipCram's saved settings.\n\
             IFS='\n'\nexec {quoted} encode --saved --notify $NAUTILUS_SCRIPT_SELECTED_FILE_PATHS\n"
        );
        let nemo = format!(
            "[Nemo Action]\nName=Squeeze with ClipCram\nComment=Encode with ClipCram's saved settings\nExec={quoted} encode --saved --notify %F\n\
             Icon-Name=clipcram\nSelection=notnone\nMimetypes={mimes}\n"
        );
        let files = linux_menu_files();
        let contents = [kde.clone(), kde, script, nemo];
        for (file, text) in files.iter().zip(contents.iter()) {
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            std::fs::write(file, text).map_err(|e| format!("{}: {e}", file.display()))?;
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755));
        }
        Ok("Added \"Squeeze with ClipCram\" to the right-click menu of Dolphin, Nautilus and Nemo. \
            You may need to restart the file manager."
            .to_string())
    }
}

pub fn uninstall_menu() -> Result<String, String> {
    #[cfg(windows)]
    {
        for ext in MENU_EXTENSIONS {
            let key = format!("HKCU\\Software\\Classes\\SystemFileAssociations\\.{ext}\\shell\\ClipCram");
            let _ = Command::new("reg").args(["delete", &key, "/f"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        return Ok("Removed the right-click menu entry.".to_string());
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(String::new());
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        for file in linux_menu_files() {
            let _ = std::fs::remove_file(file);
        }
        Ok("Removed the right-click menu entries.".to_string())
    }
}

pub fn menu_installed() -> bool {
    #[cfg(windows)]
    {
        return Command::new("reg")
            .args(["query", "HKCU\\Software\\Classes\\SystemFileAssociations\\.mp4\\shell\\ClipCram"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    }
    #[cfg(target_os = "macos")]
    {
        return false;
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        linux_menu_files().iter().any(|f| f.exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveform_peaks() {
        let samples = [0i16, 100, -200, 50, 16384, 0, 0, -8192];
        let p = peaks(samples.into_iter(), 8, 4);
        assert_eq!(p, vec![100.0 / 16384.0, 200.0 / 16384.0, 1.0, 0.5]);
        // Decoding stopping early leaves the tail empty rather than squeezing it.
        let p = peaks([1000i16; 4].into_iter(), 8, 4);
        assert_eq!(p, vec![1.0, 1.0, 0.0, 0.0]);
        assert_eq!(peaks(std::iter::empty(), 0, 3), vec![0.0; 3]);
    }

    #[test]
    fn watcher_waits_for_files_to_finish() {
        let dir = std::env::temp_dir().join(format!("clipcram-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exts = ["mp4", "mkv"];
        std::fs::write(dir.join("old.mp4"), b"old").unwrap();
        let mut w = Watcher::new(dir.clone(), &exts);
        assert!(w.poll(&exts).is_empty(), "files already there are ignored");

        std::fs::write(dir.join("new.mkv"), b"12").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        std::fs::write(dir.join(".hidden.mp4"), b"x").unwrap();
        assert!(w.poll(&exts).is_empty(), "first sighting: wait to see if it grows");
        std::fs::write(dir.join("new.mkv"), b"1234").unwrap();
        assert!(w.poll(&exts).is_empty(), "still growing");
        assert_eq!(w.poll(&exts), vec![dir.join("new.mkv")], "size settled");
        assert!(w.poll(&exts).is_empty(), "reported once");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn comparison_frames() {
        assert_eq!(compare_size(3840, 2160, 1920), (1920, 1080));
        assert_eq!(compare_size(1080, 1920, 1920), (1080, 1920));
        assert_eq!(compare_size(607, 1081, 1920), (608, 1082));
        // A real grab, when FFmpeg is installed (it is on the Linux CI runner).
        let Ok(tools) = Tools::find() else { return };
        let clip = std::env::temp_dir().join(format!("clipcram-grab-{}.mp4", std::process::id()));
        let made = Command::new(&tools.ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=30", "-t", "2", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status();
        if !made.map(|s| s.success()).unwrap_or(false) {
            return;
        }
        let frame = grab_frame(&tools, &clip, 1.0, "crop=160:180:80:0,scale=64:72", 64, 72).expect("frame");
        assert_eq!(frame.len(), 64 * 72 * 4);
        assert!(frame.iter().any(|&b| b != 0) && frame.iter().any(|&b| b != 255));
        // Asking for the wrong size (the filter makes 64x72) gives nothing rather than garbage.
        assert!(grab_frame(&tools, &clip, 1.0, "scale=64:72", 32, 32).is_none());
        let _ = std::fs::remove_file(clip);
    }

    #[test]
    fn version_comparison() {
        assert!(is_newer("v1.1.0", "1.0.0"));
        assert!(is_newer("1.0.1", "1.0"));
        assert!(!is_newer("v1.0.0", "1.0.0"));
        assert!(!is_newer("v0.9", "1.0.0"));
        assert!(!is_newer("nightly", "1.0.0"));
        assert!(is_newer("v2.0.0-beta", "1.9.9"));
    }

    #[test]
    fn release_json() {
        assert_eq!(release_tag(r#"{"tag_name":"v1.2.0","draft":false,"prerelease":false}"#), Some("v1.2.0".into()));
        assert_eq!(release_tag(r#"{"tag_name":"v2.0.0-rc1","prerelease":true}"#), None);
        assert_eq!(release_tag(r#"{"message":"Not Found"}"#), None);
        assert_eq!(release_tag("<html>"), None);
    }

    #[test]
    fn webhook_addresses() {
        assert!(webhook_url_ok("https://discord.com/api/webhooks/1/abc"));
        assert!(webhook_url_ok(" http://127.0.0.1:8080/hook "));
        assert!(!webhook_url_ok("http://example.com/hook"));
        assert!(!webhook_url_ok("discord.com/api/webhooks"));
        assert!(post_to_webhook("ftp://x", Path::new("/nonexistent"), "").is_err());
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn menu_install_and_remove() {
        // dirs::data_dir() follows XDG_DATA_HOME; point it at a scratch folder.
        let dir = std::env::temp_dir().join(format!("clipcram-menu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("XDG_DATA_HOME", &dir);
        assert!(!menu_installed());
        install_menu().unwrap();
        assert!(menu_installed());
        let kde = std::fs::read_to_string(dir.join("kio/servicemenus/clipcram.desktop")).unwrap();
        assert!(kde.contains("encode --saved --notify %F"), "{kde}");
        let script = dir.join("nautilus/scripts/Squeeze with ClipCram");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&script).unwrap().permissions().mode() & 0o111, 0o111, "scripts must be executable");
        uninstall_menu().unwrap();
        assert!(!menu_installed());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Needs a webhook address in CLIPCRAM_TEST_WEBHOOK and a file in CLIPCRAM_TEST_FILE.
    #[test]
    #[ignore]
    fn webhook_live() {
        let url = std::env::var("CLIPCRAM_TEST_WEBHOOK").unwrap();
        let file = std::env::var("CLIPCRAM_TEST_FILE").unwrap();
        let r = post_to_webhook(&url, Path::new(&file), "");
        println!("WEBHOOK {r:?}");
        if url.contains("fail") {
            assert!(r.is_err());
        } else {
            r.unwrap();
        }
    }

    #[test]
    #[ignore]
    fn update_check_live() {
        let r = newer_release("https://github.com/SimaoFonseca2/ClipCram", "1.0.0");
        println!("UPDATE {r:?}");
        assert!(r.is_ok());
        assert_eq!(newer_release("https://github.com/rust-lang/rust", "0.0.1").map(|t| t.is_some()), Ok(true));
    }

    /// Needs a display.
    #[test]
    #[ignore]
    fn clipboard_live() {
        let mut cb = None;
        let file = std::env::temp_dir().join("clipcram clip, test.mp4");
        std::fs::write(&file, b"x").unwrap();
        copy_files(&mut cb, std::slice::from_ref(&file)).unwrap();
        let back = cb.as_mut().unwrap().get().file_list().unwrap();
        println!("CLIPBOARD {back:?}");
        assert_eq!(back, vec![file]);
    }
}
