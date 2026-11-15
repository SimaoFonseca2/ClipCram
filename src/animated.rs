//! GIF and animated WebP output. Neither format has a bitrate setting, so the size is hit
//! by encoding, measuring and adjusting the picture size, frame rate and quality until
//! the file fits, keeping the largest result that does.

use crate::edit::{self, GraphSpec};
use crate::encode::{run_ffmpeg, CancelToken, JobSpec, OutputFormat, RunError, WorkerMsg};
use crate::ffmpeg::{Caps, Tools};
use crate::util;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const MAX_ATTEMPTS: usize = 7;

/// One set of picture settings to try.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shape {
    /// Longest side in pixels.
    pub long_side: u32,
    pub fps: f64,
    /// GIF palette size, or WebP quality (0-100).
    pub level: u32,
}

/// Where to start: a size and frame rate that usually lands near the limit.
pub fn first_shape(format: OutputFormat, src_long: u32, src_fps: f64) -> Shape {
    match format {
        OutputFormat::Gif => Shape { long_side: src_long.min(480), fps: src_fps.min(15.0), level: 256 },
        _ => Shape { long_side: src_long.min(720), fps: src_fps.min(20.0), level: 75 },
    }
}

/// The next shape after a result of `size` bytes against `target`. Returns None when
/// there's nothing sensible left to try.
pub fn next_shape(format: OutputFormat, shape: Shape, size: u64, target: u64, src_long: u32) -> Option<Shape> {
    let ratio = target as f64 / size.max(1) as f64;
    let mut s = shape;
    if ratio >= 1.0 {
        // Fits. Grow once if there's lots of room and the source allows it.
        if ratio < 1.6 || shape.long_side >= src_long {
            return None;
        }
        s.long_side = ((shape.long_side as f64 * ratio.sqrt() * 0.92) as u32).min(src_long);
        return (s.long_side > shape.long_side + 16).then_some(s);
    }
    // Too big. WebP first gives up quality, both then shrink the picture; very small
    // pictures also drop frames.
    if format == OutputFormat::Webp && s.level > 40 {
        s.level = (s.level - 15).max(40);
        if ratio > 0.75 {
            return Some(s);
        }
    }
    s.long_side = (shape.long_side as f64 * ratio.sqrt() * 0.92) as u32;
    if s.long_side < 240 && s.fps > 10.0 {
        s.fps = 10.0;
        s.long_side = (s.long_side as f64 * 1.15) as u32;
    }
    if format == OutputFormat::Gif && s.long_side < 200 {
        s.level = 128;
    }
    (s.long_side >= 64).then_some(s)
}

pub fn run(
    tools: &Tools,
    caps: &Caps,
    spec: &JobSpec,
    tmp: &Path,
    cancel: &CancelToken,
    send: &dyn Fn(WorkerMsg),
) -> Result<(PathBuf, u64), RunError> {
    let format = spec.settings.format;
    if format == OutputFormat::Webp && caps.webp.is_none() {
        return Err(RunError::Failed("This FFmpeg build has no WebP encoder (libwebp).".to_string()));
    }
    let out = spec.out_info();
    let src_long = out.width.max(out.height);
    let name = util::sanitize_filename(&crate::encode::with_format_extension(&spec.output_name, format));
    let mut shape = first_shape(format, src_long, out.fps);
    let mut best: Option<(PathBuf, u64)> = None;
    let mut partials: Vec<PathBuf> = Vec::new();

    for attempt in 0..MAX_ATTEMPTS {
        send(WorkerMsg::Status(format!(
            "{} attempt {}: longest side {} px, {} fps",
            if format == OutputFormat::Gif { "GIF" } else { "WebP" },
            attempt + 1,
            shape.long_side,
            util::trim_float(shape.fps)
        )));
        let partial = spec.output_dir.join(format!(".{name}.part{attempt}"));
        partials.push(partial.clone());
        let args = build_args(caps, spec, shape, &partial);
        let mut on_progress = |f: f64, speed: Option<f64>| {
            send(WorkerMsg::Progress {
                fraction: ((attempt as f64 + f) / MAX_ATTEMPTS as f64).min(0.97) as f32,
                speed,
                eta_secs: None,
            })
        };
        let result = run_ffmpeg(tools, &args, tmp, cancel, out.duration, &mut on_progress);
        if let Err(e) = result {
            for p in &partials {
                if best.as_ref().map(|(b, _)| b) != Some(p) {
                    let _ = std::fs::remove_file(p);
                }
            }
            if let Some((b, _)) = &best {
                let _ = std::fs::remove_file(b);
            }
            return Err(e);
        }
        let size = std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(u64::MAX);
        send(WorkerMsg::Status(format!("Output: {:.2} MiB", util::mib(size))));
        let fits = size <= spec.target_bytes;
        if fits && best.as_ref().is_none_or(|(_, s)| size > *s) {
            if let Some((old, _)) = best.replace((partial.clone(), size)) {
                let _ = std::fs::remove_file(old);
            }
        } else {
            let _ = std::fs::remove_file(&partial);
        }
        if fits && best.as_ref().map(|(p, _)| p) != Some(&partial) {
            break; // A bigger attempt that fit is already kept.
        }
        match next_shape(format, shape, size, spec.target_bytes, src_long) {
            Some(next) if !(fits && best.is_some() && next.long_side <= shape.long_side) => shape = next,
            _ => break,
        }
    }

    let Some((partial, size)) = best else {
        return Err(RunError::Failed(format!(
            "Could not get the {} below {}. Try a shorter clip.",
            format.ext().to_uppercase(),
            util::format_size(spec.target_bytes as f64)
        )));
    };
    let final_path = util::unique_output_path(&spec.output_dir, &name);
    std::fs::rename(&partial, &final_path)
        .or_else(|_| std::fs::copy(&partial, &final_path).map(|_| ()))
        .map_err(|e| RunError::Failed(format!("Could not save {}: {e}", final_path.display())))?;
    let _ = std::fs::remove_file(&partial);
    Ok((final_path, size))
}

fn build_args(caps: &Caps, spec: &JobSpec, shape: Shape, output: &Path) -> Vec<OsString> {
    let out = spec.out_info();
    let landscape = out.width >= out.height;
    let scale = if landscape {
        format!("scale={}:-2:flags=lanczos", shape.long_side.min(out.width) & !1)
    } else {
        format!("scale=-2:{}:flags=lanczos", shape.long_side.min(out.height) & !1)
    };
    let mut vf: Vec<String> = Vec::new();
    if let Some(c) = spec.edit.effective_crop(spec.info.width, spec.info.height) {
        vf.push(format!("crop={}:{}:{}:{}", c.w, c.h, c.x, c.y));
    }
    vf.push(format!("fps={}", util::trim_float(shape.fps)));
    vf.push(scale);
    if caps.drawtext {
        // The caption needs the output height; estimate it from the scale.
        let (w, h) = if landscape {
            (shape.long_side, (shape.long_side as f64 * out.height as f64 / out.width as f64) as u32)
        } else {
            ((shape.long_side as f64 * out.width as f64 / out.height as f64) as u32, shape.long_side)
        };
        if let Some(c) = edit::caption_filter(&spec.edit, w, h, edit::caption_font().as_deref()) {
            vf.push(c);
        }
    }
    let graph = edit::build_graph(&GraphSpec {
        input: 0,
        video_index: spec.info.video_index,
        start: spec.start,
        end: spec.end,
        edit: &spec.edit,
        vf: vf.join(","),
        audio: None,
    });

    let mut a: Vec<OsString> = Vec::new();
    let mut push = |v: &str| a.push(v.into());
    for v in ["-hide_banner", "-nostdin", "-y", "-v", "error", "-nostats", "-progress", "pipe:1", "-ss"] {
        push(v);
    }
    push(&format!("{:.3}", spec.start));
    push("-t");
    push(&format!("{:.3}", spec.end - spec.start));
    push("-i");
    a.push(spec.input.clone().into_os_string());
    let mut push = |v: &str| a.push(v.into());
    match spec.settings.format {
        OutputFormat::Gif => {
            // A palette made for this clip, and ordered dithering, which compresses far
            // better in GIF than error diffusion.
            let filter = format!(
                "{};{}split[g1][g2];[g1]palettegen=max_colors={}:stats_mode=diff[pal];\
                 [g2][pal]paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle[out]",
                graph.filter, graph.video, shape.level
            );
            for v in ["-filter_complex", &filter, "-map", "[out]", "-an", "-loop", "0", "-f", "gif"] {
                push(v);
            }
        }
        _ => {
            let encoder = caps.webp.clone().unwrap_or_else(|| "libwebp_anim".to_string());
            let q = shape.level.to_string();
            for v in [
                "-filter_complex", &graph.filter, "-map", &graph.video, "-an", "-c:v", &encoder, "-lossless", "0",
                "-q:v", &q, "-compression_level", "4", "-loop", "0", "-f", "webp",
            ] {
                push(v);
            }
        }
    }
    a.push(output.as_os_str().to_owned());
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_shrink_until_they_fit() {
        let s = first_shape(OutputFormat::Gif, 1920, 60.0);
        assert_eq!(s, Shape { long_side: 480, fps: 15.0, level: 256 });
        // Twice too big: shrink each side by about 1/sqrt(2).
        let n = next_shape(OutputFormat::Gif, s, 20_000_000, 10_000_000, 1920).unwrap();
        assert!(n.long_side < 340 && n.long_side > 290 && n.fps == 15.0, "{n:?}");
        // Four times too big: small enough that it also drops to 10 fps.
        let n = next_shape(OutputFormat::Gif, s, 40_000_000, 10_000_000, 1920).unwrap();
        assert!(n.long_side < 270 && n.long_side > 200 && n.fps == 10.0, "{n:?}");
        // WebP lowers quality before shrinking when it's only a little too big.
        let w = first_shape(OutputFormat::Webp, 1920, 60.0);
        let n = next_shape(OutputFormat::Webp, w, 11_000_000, 10_000_000, 1920).unwrap();
        assert_eq!((n.long_side, n.level), (720, 60));
        // Fits with lots of room: grows once, but never past the source.
        let n = next_shape(OutputFormat::Gif, s, 2_000_000, 10_000_000, 600).unwrap();
        assert_eq!(n.long_side, 600);
        assert!(next_shape(OutputFormat::Gif, s, 9_000_000, 10_000_000, 1920).is_none());
        // Hopelessly too big gives up instead of making a postage stamp.
        assert!(next_shape(OutputFormat::Gif, Shape { long_side: 70, fps: 10.0, level: 128 }, 100_000_000, 1_000_000, 1920).is_none());
    }
}
