//! Per-video edits beyond the start and end points: parts cut out, parts muted, a crop,
//! a speed change, a caption, and per-track volume. Also builds the FFmpeg filter graph
//! that applies them, shared by the MP4 encoder, the GIF/WebP encoder and the quality check.
//!
//! All times here are source times in seconds, like the start and end points. The encoder
//! seeks the input to the start point, so inside the filter graph a source time `t` is
//! `t - start`.

use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

/// Parts shorter than this are dropped: they'd be a stray frame or two.
pub const MIN_PART: f64 = 0.05;

pub const SPEEDS: [f64; 8] = [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 4.0];

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Range {
    pub start: f64,
    pub end: f64,
}

impl Range {
    pub fn new(a: f64, b: f64) -> Range {
        Range { start: a.min(b), end: a.max(b) }
    }

    pub fn len(&self) -> f64 {
        (self.end - self.start).max(0.0)
    }
}

/// Sorted, non-overlapping ranges with tiny ones removed.
fn normalize(mut ranges: Vec<Range>) -> Vec<Range> {
    ranges.retain(|r| r.len() >= MIN_PART && r.start.is_finite() && r.end.is_finite());
    ranges.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut out: Vec<Range> = Vec::new();
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.start <= last.end + 1e-6 => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// A crop rectangle in display pixels (after phone rotation is applied).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Crop {
    /// Keeps the rectangle inside the frame, at least 32 pixels each way and with even
    /// sizes and offsets, which 4:2:0 video needs.
    pub fn clamped(self, src_w: u32, src_h: u32) -> Crop {
        let even = |v: u32| v & !1;
        let w = even(self.w.clamp(32.min(src_w), src_w));
        let h = even(self.h.clamp(32.min(src_h), src_h));
        let x = even(self.x.min(src_w - w));
        let y = even(self.y.min(src_h - h));
        Crop { x, y, w, h }
    }

    /// The largest centered rectangle with the given aspect ratio.
    pub fn centered(src_w: u32, src_h: u32, aspect_w: u32, aspect_h: u32) -> Crop {
        let (sw, sh) = (src_w as f64, src_h as f64);
        let aspect = aspect_w as f64 / aspect_h.max(1) as f64;
        let (w, h) = if sw / sh > aspect { (sh * aspect, sh) } else { (sw, sw / aspect) };
        let (w, h) = (w.floor() as u32, h.floor() as u32);
        Crop { x: (src_w - w) / 2, y: (src_h - h) / 2, w, h }.clamped(src_w, src_h)
    }

    pub fn is_full(&self, src_w: u32, src_h: u32) -> bool {
        self.x == 0 && self.y == 0 && self.w >= src_w.saturating_sub(1) && self.h >= src_h.saturating_sub(1)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextPos {
    #[default]
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl TextSize {
    /// Font size as a share of the output height.
    fn factor(self) -> f64 {
        match self {
            TextSize::Small => 0.045,
            TextSize::Medium => 0.065,
            TextSize::Large => 0.09,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Edit {
    /// Parts removed from the clip.
    pub cuts: Vec<Range>,
    /// Parts where the audio is silenced.
    pub mutes: Vec<Range>,
    pub crop: Option<Crop>,
    /// Playback speed: 2.0 is twice as fast, 0.5 slow motion.
    pub speed: f64,
    /// Caption drawn over the video; empty for none.
    pub text: String,
    pub text_pos: TextPos,
    pub text_size: TextSize,
    /// Volume per source audio track, used when tracks are mixed; missing entries are 1.0.
    pub track_volumes: Vec<f32>,
}

impl Default for Edit {
    fn default() -> Self {
        Edit {
            cuts: Vec::new(),
            mutes: Vec::new(),
            crop: None,
            speed: 1.0,
            text: String::new(),
            text_pos: TextPos::Top,
            text_size: TextSize::Medium,
            track_volumes: Vec::new(),
        }
    }
}

impl Edit {
    /// The parts of start..end that are kept, in order.
    pub fn kept(&self, start: f64, end: f64) -> Vec<Range> {
        let mut out = Vec::new();
        let mut at = start;
        for cut in normalize(self.cuts.clone()) {
            if cut.end <= at || cut.start >= end {
                continue;
            }
            if cut.start > at {
                out.push(Range { start: at, end: cut.start.min(end) });
            }
            at = at.max(cut.end);
        }
        if end > at {
            out.push(Range { start: at, end });
        }
        out.retain(|r| r.len() >= MIN_PART);
        out
    }

    /// Length of the finished video.
    pub fn output_duration(&self, start: f64, end: f64) -> f64 {
        self.kept(start, end).iter().map(Range::len).sum::<f64>() / self.speed()
    }

    pub fn speed(&self) -> f64 {
        if self.speed.is_finite() && self.speed > 0.0 {
            self.speed.clamp(0.25, 4.0)
        } else {
            1.0
        }
    }

    pub fn volume(&self, track: usize) -> f32 {
        self.track_volumes.get(track).copied().unwrap_or(1.0).clamp(0.0, 4.0)
    }

    pub fn add_cut(&mut self, r: Range) {
        self.cuts.push(r);
        self.cuts = normalize(std::mem::take(&mut self.cuts));
    }

    pub fn add_mute(&mut self, r: Range) {
        self.mutes.push(r);
        self.mutes = normalize(std::mem::take(&mut self.mutes));
    }

    pub fn has_caption(&self) -> bool {
        !self.text.trim().is_empty()
    }

    fn has_volume_changes(&self) -> bool {
        self.track_volumes.iter().any(|v| (v - 1.0).abs() > 0.001)
    }

    /// Whether the video needs anything beyond trimming (used to decide if a stream copy
    /// can stand in for an encode).
    pub fn is_plain(&self, start: f64, end: f64, src_w: u32, src_h: u32) -> bool {
        self.kept(start, end).len() == 1
            && self.kept(start, end)[0].len() >= (end - start) - 1e-6
            && (self.speed() - 1.0).abs() < 1e-6
            && self.mutes.iter().all(|m| m.end <= start || m.start >= end)
            && self.crop.is_none_or(|c| c.is_full(src_w, src_h))
            && !self.has_caption()
            && !self.has_volume_changes()
    }

    /// The crop to apply, if any, clamped to the frame.
    pub fn effective_crop(&self, src_w: u32, src_h: u32) -> Option<Crop> {
        self.crop.map(|c| c.clamped(src_w, src_h)).filter(|c| !c.is_full(src_w, src_h))
    }
}

/// `atempo` only takes factors between 0.5 and 2 in older FFmpeg builds, so larger
/// changes are chained.
pub fn atempo_chain(speed: f64) -> Vec<f64> {
    let mut s = speed;
    let mut out = Vec::new();
    while s > 2.0 + 1e-9 {
        out.push(2.0);
        s /= 2.0;
    }
    while s < 0.5 - 1e-9 {
        out.push(0.5);
        s /= 0.5;
    }
    if (s - 1.0).abs() > 1e-9 || out.is_empty() {
        out.push(s);
    }
    out
}

/// How audio tracks are handled in the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioLayout {
    /// All source tracks mixed into one.
    Mix,
    /// Only the first track.
    First,
    /// Every track, each processed on its own.
    KeepAll,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioPlan {
    pub source_tracks: usize,
    pub layout: AudioLayout,
    /// EBU R128 loudness normalization to -16 LUFS, the common target for online video.
    pub normalize: bool,
}

pub struct GraphSpec<'a> {
    /// FFmpeg input number of the source (0 unless other inputs come first).
    pub input: usize,
    pub video_index: usize,
    pub start: f64,
    pub end: f64,
    pub edit: &'a Edit,
    /// Per-frame filters after cutting and speed (crop, scale, caption, pixel format...).
    /// Empty for none.
    pub vf: String,
    /// None when the output has no audio (first pass, audio off, GIF).
    pub audio: Option<AudioPlan>,
}

pub struct Graph {
    pub filter: String,
    pub video: String,
    pub audio: Vec<String>,
}

/// The `-filter_complex` graph for a clip. The input is expected to be opened with `-ss start`.
pub fn build_graph(spec: &GraphSpec) -> Graph {
    let edit = spec.edit;
    let kept: Vec<Range> = edit
        .kept(spec.start, spec.end)
        .into_iter()
        .map(|r| Range { start: r.start - spec.start, end: r.end - spec.start })
        .collect();
    let speed = edit.speed();
    let n = kept.len().max(1);
    let mut g = String::new();

    // Video: cut into parts and join them, change speed, then the per-frame filters.
    let (inp, vi) = (spec.input, spec.video_index);
    let mut tail: Vec<String> = Vec::new();
    if (speed - 1.0).abs() > 1e-9 {
        tail.push(format!("setpts=PTS/{speed}"));
    }
    if !spec.vf.is_empty() {
        tail.push(spec.vf.clone());
    }
    let tail = if tail.is_empty() { "null".to_string() } else { tail.join(",") };
    if n > 1 {
        let _ = write!(g, "[{inp}:{vi}]split={n}");
        for i in 0..n {
            let _ = write!(g, "[vs{i}]");
        }
        g.push(';');
        for (i, r) in kept.iter().enumerate() {
            let _ = write!(g, "[vs{i}]trim=start={:.6}:end={:.6},setpts=PTS-STARTPTS[vp{i}];", r.start, r.end);
        }
        for i in 0..n {
            let _ = write!(g, "[vp{i}]");
        }
        let _ = write!(g, "concat=n={n}:v=1:a=0,{tail}[v]");
    } else {
        let _ = write!(g, "[{inp}:{vi}]{tail}[v]");
    }

    let mut audio = Vec::new();
    if let Some(plan) = spec.audio.filter(|p| p.source_tracks > 0) {
        // Sources: one chain per output track.
        let mut chains: Vec<String> = Vec::new();
        let with_volume = |track: usize| {
            let v = edit.volume(track);
            if (v - 1.0).abs() > 0.001 {
                format!("[{inp}:a:{track}]volume={v:.3}")
            } else {
                format!("[{inp}:a:{track}]anull")
            }
        };
        match plan.layout {
            AudioLayout::Mix if plan.source_tracks > 1 => {
                for t in 0..plan.source_tracks {
                    let _ = write!(g, ";{}[am{t}]", with_volume(t));
                }
                g.push(';');
                for t in 0..plan.source_tracks {
                    let _ = write!(g, "[am{t}]");
                }
                let _ = write!(g, "amix=inputs={}:duration=longest:normalize=0[amixed]", plan.source_tracks);
                chains.push("[amixed]anull".to_string());
            }
            AudioLayout::KeepAll => {
                for t in 0..plan.source_tracks {
                    chains.push(with_volume(t));
                }
            }
            _ => chains.push(with_volume(0)),
        }

        let mutes: Vec<String> = normalize(edit.mutes.clone())
            .iter()
            .filter(|m| m.end > spec.start && m.start < spec.end)
            .map(|m| format!("between(t,{:.6},{:.6})", m.start - spec.start, m.end - spec.start))
            .collect();

        for (k, head) in chains.iter().enumerate() {
            let mut steps = vec![head.clone()];
            if !mutes.is_empty() {
                steps.push(format!("volume=0:enable='{}'", mutes.join("+")));
            }
            let pre = steps.join(",");
            let mut post: Vec<String> = Vec::new();
            if (speed - 1.0).abs() > 1e-9 {
                post.extend(atempo_chain(speed).iter().map(|f| format!("atempo={f}")));
            }
            if plan.normalize {
                // Single-pass loudnorm works at 192 kHz internally; bring it back down.
                post.push("loudnorm=I=-16:TP=-1.5:LRA=11".to_string());
                post.push("aresample=48000".to_string());
            }
            let post = if post.is_empty() { "anull".to_string() } else { post.join(",") };
            let label = format!("[a{k}]");
            if n > 1 {
                let _ = write!(g, ";{pre},asplit={n}");
                for i in 0..n {
                    let _ = write!(g, "[as{k}_{i}]");
                }
                for (i, r) in kept.iter().enumerate() {
                    let _ = write!(
                        g,
                        ";[as{k}_{i}]atrim=start={:.6}:end={:.6},asetpts=PTS-STARTPTS[ap{k}_{i}]",
                        r.start, r.end
                    );
                }
                g.push(';');
                for i in 0..n {
                    let _ = write!(g, "[ap{k}_{i}]");
                }
                let _ = write!(g, "concat=n={n}:v=0:a=1,{post}{label}");
            } else {
                let _ = write!(g, ";{pre},{post}{label}");
            }
            audio.push(label);
        }
    }

    Graph { filter: g, video: "[v]".to_string(), audio }
}

/// Whether an encode needs the full filter graph rather than a plain `-vf` chain.
pub fn needs_graph(edit: &Edit, start: f64, end: f64, audio: Option<AudioPlan>) -> bool {
    edit.kept(start, end).len() > 1
        || (edit.speed() - 1.0).abs() > 1e-9
        || audio.is_some_and(|a| {
            a.normalize
                || (a.layout == AudioLayout::Mix && a.source_tracks > 1)
                || !normalize(edit.mutes.clone()).iter().all(|m| m.end <= start || m.start >= end)
                || edit.has_volume_changes()
        })
}

/// The caption filter. Reads the text from `caption.txt` in FFmpeg's working folder, so
/// no quoting rules apply to what the user typed.
pub fn caption_filter(edit: &Edit, out_w: u32, out_h: u32, font: Option<&str>) -> Option<String> {
    if !edit.has_caption() {
        return None;
    }
    let chars = edit.text.trim().chars().count().max(1) as f64;
    // Shrink long captions so they fit across the frame (about 0.55 em per character).
    let size = (out_h as f64 * edit.text_size.factor()).min(out_w as f64 * 0.92 / (chars * 0.55)).max(10.0);
    let border = (size / 12.0).max(1.0);
    let margin = (out_h as f64 * 0.05).round();
    let y = match edit.text_pos {
        TextPos::Top => format!("{margin}"),
        TextPos::Bottom => format!("h-text_h-{margin}"),
    };
    let font = font.map(|f| format!("fontfile={f}:")).unwrap_or_default();
    Some(format!(
        "drawtext={font}textfile=caption.txt:fontcolor=white:fontsize={size:.0}:borderw={border:.1}:\
         bordercolor=black@0.85:x=(w-text_w)/2:y={y}"
    ))
}

/// A font file for captions on systems where FFmpeg may lack fontconfig. Paths avoid
/// characters with a meaning in filter graphs (on Windows the drive letter is left off;
/// it resolves against the drive of FFmpeg's working folder, the system temp folder).
pub fn caption_font() -> Option<String> {
    let candidates: &[&str] = if cfg!(windows) {
        &["/Windows/Fonts/arialbd.ttf", "/Windows/Fonts/segoeuib.ttf", "/Windows/Fonts/arial.ttf"]
    } else if cfg!(target_os = "macos") {
        &["/System/Library/Fonts/Helvetica.ttc", "/Library/Fonts/Arial.ttf"]
    } else {
        &[
            "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/noto/NotoSans-Bold.ttf",
            "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
            "/usr/share/fonts/liberation/LiberationSans-Bold.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
            "/usr/share/fonts/TTF/LiberationSans-Bold.ttf",
        ]
    };
    let exists = |p: &&str| {
        if cfg!(windows) {
            std::path::Path::new(&format!("C:{p}")).exists() || std::path::Path::new(p).exists()
        } else {
            std::path::Path::new(p).exists()
        }
    };
    candidates.iter().copied().find(|p| exists(p)).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(a: f64, b: f64) -> Range {
        Range::new(a, b)
    }

    #[test]
    fn kept_parts() {
        let mut e = Edit::default();
        assert_eq!(e.kept(0.0, 10.0), vec![r(0.0, 10.0)]);
        e.add_cut(r(2.0, 4.0));
        e.add_cut(r(3.0, 5.0)); // overlaps, merges into 2..5
        e.add_cut(r(8.0, 12.0)); // runs past the end
        assert_eq!(e.cuts, vec![r(2.0, 5.0), r(8.0, 12.0)]);
        assert_eq!(e.kept(0.0, 10.0), vec![r(0.0, 2.0), r(5.0, 8.0)]);
        assert!((e.output_duration(0.0, 10.0) - 5.0).abs() < 1e-9);
        // Trimming the start past a cut.
        assert_eq!(e.kept(3.0, 10.0), vec![r(5.0, 8.0)]);
        // A cut covering everything leaves nothing.
        e.add_cut(r(0.0, 20.0));
        assert!(e.kept(0.0, 10.0).is_empty());
    }

    #[test]
    fn speed_changes_duration() {
        let e = Edit { speed: 2.0, ..Edit::default() };
        assert!((e.output_duration(0.0, 10.0) - 5.0).abs() < 1e-9);
        let e = Edit { speed: 0.5, ..Edit::default() };
        assert!((e.output_duration(0.0, 10.0) - 20.0).abs() < 1e-9);
        // Garbage speeds fall back to normal.
        assert_eq!(Edit { speed: 0.0, ..Edit::default() }.speed(), 1.0);
    }

    #[test]
    fn atempo_chains_stay_in_range() {
        assert_eq!(atempo_chain(4.0), vec![2.0, 2.0]);
        assert_eq!(atempo_chain(0.25), vec![0.5, 0.5]);
        assert_eq!(atempo_chain(1.5), vec![1.5]);
        for s in SPEEDS {
            let chain = atempo_chain(s);
            assert!(chain.iter().all(|f| (0.5..=2.0).contains(f)), "{s}: {chain:?}");
            assert!((chain.iter().product::<f64>() - s).abs() < 1e-9);
        }
    }

    #[test]
    fn crop_helpers() {
        let c = Crop::centered(1920, 1080, 9, 16);
        assert_eq!((c.w, c.h), (606, 1080));
        assert_eq!(c.x, 656);
        let c = Crop::centered(1920, 1080, 1, 1);
        assert_eq!((c.x, c.y, c.w, c.h), (420, 0, 1080, 1080));
        // Odd and out-of-frame values are fixed up.
        let c = Crop { x: 1901, y: 3, w: 101, h: 2000 }.clamped(1920, 1080);
        assert_eq!(c, Crop { x: 1820, y: 0, w: 100, h: 1080 });
        assert_eq!(Crop { x: 0, y: 0, w: 5, h: 5 }.clamped(1920, 1080), Crop { x: 0, y: 0, w: 32, h: 32 });
        assert!(Crop { x: 0, y: 0, w: 1920, h: 1080 }.is_full(1920, 1080));
    }

    #[test]
    fn plain_and_graph_decisions() {
        let e = Edit::default();
        assert!(e.is_plain(0.0, 10.0, 1920, 1080));
        let mix = Some(AudioPlan { source_tracks: 2, layout: AudioLayout::Mix, normalize: false });
        let first = Some(AudioPlan { source_tracks: 2, layout: AudioLayout::First, normalize: false });
        assert!(needs_graph(&e, 0.0, 10.0, mix));
        assert!(!needs_graph(&e, 0.0, 10.0, first));
        assert!(!needs_graph(&e, 0.0, 10.0, None));
        let mut cut = Edit::default();
        cut.add_cut(r(2.0, 3.0));
        assert!(needs_graph(&cut, 0.0, 10.0, None));
        assert!(!cut.is_plain(0.0, 10.0, 1920, 1080));
        // A cut outside the trimmed range changes nothing.
        assert!(!needs_graph(&cut, 5.0, 10.0, None));
        let muted = Edit { mutes: vec![r(1.0, 2.0)], ..Edit::default() };
        assert!(needs_graph(&muted, 0.0, 10.0, first));
        assert!(!needs_graph(&muted, 0.0, 10.0, None));
    }

    #[test]
    fn graph_for_cuts_speed_and_audio() {
        let mut e = Edit { speed: 2.0, mutes: vec![r(11.0, 12.0)], track_volumes: vec![1.0, 0.5], ..Edit::default() };
        e.add_cut(r(13.0, 15.0));
        let g = build_graph(&GraphSpec {
            input: 0,
            video_index: 0,
            start: 10.0,
            end: 20.0,
            edit: &e,
            vf: "scale=640:360".into(),
            audio: Some(AudioPlan { source_tracks: 2, layout: AudioLayout::Mix, normalize: true }),
        });
        let f = &g.filter;
        assert!(f.starts_with("[0:0]split=2[vs0][vs1];"), "{f}");
        assert!(f.contains("[vs0]trim=start=0.000000:end=3.000000"), "{f}");
        assert!(f.contains("[vs1]trim=start=5.000000:end=10.000000"), "{f}");
        assert!(f.contains("concat=n=2:v=1:a=0,setpts=PTS/2,scale=640:360[v]"), "{f}");
        assert!(f.contains("[0:a:1]volume=0.500[am1]"), "{f}");
        assert!(f.contains("amix=inputs=2"), "{f}");
        assert!(f.contains("volume=0:enable='between(t,1.000000,2.000000)'"), "{f}");
        assert!(f.contains("atempo=2,loudnorm"), "{f}");
        assert_eq!(g.audio, vec!["[a0]"]);
    }

    #[test]
    fn graph_keep_all_tracks_and_no_audio() {
        let e = Edit::default();
        let spec = |audio| GraphSpec { input: 0, video_index: 1, start: 0.0, end: 5.0, edit: &e, vf: String::new(), audio };
        let g = build_graph(&spec(Some(AudioPlan { source_tracks: 2, layout: AudioLayout::KeepAll, normalize: false })));
        assert_eq!(g.audio, vec!["[a0]", "[a1]"]);
        assert!(g.filter.starts_with("[0:1]null[v]"), "{}", g.filter);
        let g = build_graph(&spec(None));
        assert_eq!(g.filter, "[0:1]null[v]");
        assert!(g.audio.is_empty());
    }

    #[test]
    fn caption_sizes() {
        let e = Edit { text: "gg".into(), text_pos: TextPos::Bottom, ..Edit::default() };
        let f = caption_filter(&e, 1280, 720, Some("/f.ttf")).unwrap();
        assert!(f.starts_with("drawtext=fontfile=/f.ttf:textfile=caption.txt"), "{f}");
        assert!(f.contains("fontsize=47"), "{f}");
        assert!(f.contains("y=h-text_h-36"), "{f}");
        // A long caption shrinks to fit the width.
        let long = Edit { text: "x".repeat(80), ..Edit::default() };
        let f = caption_filter(&long, 1280, 720, None).unwrap();
        assert!(f.contains("fontsize=27"), "{f}");
        assert!(caption_filter(&Edit::default(), 1280, 720, None).is_none());
    }
}
