//! Small helpers shared across the app: time formatting and output file names.

use std::path::{Path, PathBuf};

/// `m:ss` or `h:mm:ss`, whole seconds.
pub fn format_time(seconds: f64) -> String {
    let s = if seconds.is_finite() && seconds > 0.0 {
        seconds as u64
    } else {
        0
    };
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// `m:ss.cc` or `h:mm:ss.cc`, for trim points where hundredths matter.
pub fn format_time_precise(seconds: f64) -> String {
    let total_cs = if seconds.is_finite() && seconds > 0.0 {
        (seconds * 100.0).round() as u64
    } else {
        0
    };
    let (s, cs) = (total_cs / 100, total_cs % 100);
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}.{cs:02}")
    } else {
        format!("{m}:{sec:02}.{cs:02}")
    }
}

/// Accepts plain seconds ("83.5", "83.5 sec") or clock time ("1:23.5", "0:01:23").
pub fn parse_time(text: &str) -> Option<f64> {
    let t = text
        .trim()
        .trim_end_matches(|c: char| c.is_alphabetic() || c.is_whitespace());
    if t.is_empty() {
        return None;
    }
    let parts: Vec<&str> = t.split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    let mut total = 0.0;
    for part in parts {
        let value: f64 = part.trim().parse().ok()?;
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        total = total * 60.0 + value;
    }
    Some(total)
}

/// Trims whitespace, replaces characters that are illegal on Windows,
/// and makes sure the name ends in `.mp4`.
pub fn sanitize_filename(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return "discord_clip.mp4".to_string();
    }
    let mut out: String = name
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 0x20 {
                '_'
            } else {
                c
            }
        })
        .collect();
    if !out.to_lowercase().ends_with(".mp4") {
        out.push_str(".mp4");
    }
    out
}

/// Never overwrite an existing file: `name.mp4`, then `name_1.mp4`, `name_2.mp4`, ...
pub fn unique_output_path(folder: &Path, filename: &str) -> PathBuf {
    let filename = sanitize_filename(filename);
    let candidate = folder.join(&filename);
    if !candidate.exists() {
        return candidate;
    }
    let as_path = Path::new(&filename);
    let stem = as_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "discord_clip".to_string());
    let suffix = as_path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let mut counter = 1u32;
    loop {
        let candidate = folder.join(format!("{stem}_{counter}{suffix}"));
        if !candidate.exists() {
            return candidate;
        }
        counter += 1;
    }
}

/// Expands a leading `~` to the home folder.
pub fn expand_tilde(path: &str) -> PathBuf {
    let path = path.trim();
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// The OS's videos folder, falling back to `~/Videos`.
pub fn default_videos_dir() -> String {
    dirs::video_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Videos")))
        .unwrap_or_else(|| PathBuf::from("."))
        .to_string_lossy()
        .into_owned()
}

pub fn file_name_lossy(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

pub fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Formats an integer with thousands separators, e.g. 2450 -> "2,450".
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_round_trip() {
        assert_eq!(format_time(0.0), "0:00");
        assert_eq!(format_time(83.9), "1:23");
        assert_eq!(format_time(3723.0), "1:02:03");
        assert_eq!(format_time_precise(83.5), "1:23.50");
        assert_eq!(parse_time("83.5"), Some(83.5));
        assert_eq!(parse_time("1:23.5"), Some(83.5));
        assert_eq!(parse_time("0:01:23"), Some(83.0));
        assert_eq!(parse_time("12 sec"), Some(12.0));
        assert_eq!(parse_time("abc"), None);
    }

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_filename("  "), "discord_clip.mp4");
        assert_eq!(sanitize_filename("a:b?c"), "a_b_c.mp4");
        assert_eq!(sanitize_filename("clip.MP4"), "clip.MP4");
    }

    #[test]
    fn separators() {
        assert_eq!(thousands(2450), "2,450");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1234567), "1,234,567");
    }
}
