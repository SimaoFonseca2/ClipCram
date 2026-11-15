//! The timeline under the preview: a time ruler, thumbnails, the audio waveform, the trim
//! handles, parts that are cut out (red) or muted (blue), and the playhead. Ctrl+scroll
//! (or a pinch) zooms around the pointer; scrolling sideways pans when zoomed in.

use super::{App, DragTarget, MarkKind, Status, MIN_CLIP};
use crate::util;
use eframe::egui::{self, Color32, CursorIcon, FontId, Id, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};

const RULER_H: f32 = 16.0;
const THUMB_H: f32 = 40.0;
const WAVE_H: f32 = 26.0;
const HANDLE_GRAB: f32 = 8.0;

/// Seconds between ruler ticks so labels stay at least `min_px` apart.
pub fn tick_step(view_len: f64, width: f32, min_px: f32) -> f64 {
    const STEPS: [f64; 14] = [0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0];
    let per_px = view_len / width.max(1.0) as f64;
    STEPS.into_iter().find(|s| s / per_px >= min_px as f64).unwrap_or(3600.0)
}

/// Zooms the visible range by `factor` (>1 zooms in) around time `at`, within 0..duration.
pub fn zoom_view(view: (f64, f64), at: f64, factor: f64, duration: f64) -> (f64, f64) {
    let len = ((view.1 - view.0) / factor).clamp((duration / 400.0).max(0.5).min(duration), duration);
    let frac = if view.1 > view.0 { ((at - view.0) / (view.1 - view.0)).clamp(0.0, 1.0) } else { 0.5 };
    let start = (at - len * frac).clamp(0.0, (duration - len).max(0.0));
    (start, start + len)
}

impl App {
    pub(super) fn timeline(&mut self, ui: &mut Ui) {
        let p = self.palette();
        let Some(index) = self.selected_index() else { return };
        let sample = self.showing_sample.is_some();
        let duration = self.items[index].info.duration.max(0.001);
        let mut view = self.view.unwrap_or((0.0, duration));

        let height = RULER_H + THUMB_H + WAVE_H + 6.0;
        let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click_and_drag());
        let track = Rect::from_min_max(Pos2::new(rect.left() + 6.0, rect.top() + RULER_H), Pos2::new(rect.right() - 6.0, rect.bottom() - 2.0));

        // Zoom and pan.
        if response.hovered() {
            let (zoom, scroll) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta));
            if (zoom - 1.0).abs() > 1e-3 {
                let at = response
                    .hover_pos()
                    .map(|p| view.0 + ((p.x - track.left()) / track.width()).clamp(0.0, 1.0) as f64 * (view.1 - view.0))
                    .unwrap_or((view.0 + view.1) / 2.0);
                view = zoom_view(view, at, zoom as f64, duration);
            }
            let pan = if scroll.x.abs() > 0.0 { scroll.x } else if ui.input(|i| i.modifiers.shift) { scroll.y } else { 0.0 };
            if pan != 0.0 && view.1 - view.0 < duration {
                let shift = -(pan as f64) / track.width() as f64 * (view.1 - view.0);
                let len = view.1 - view.0;
                let start = (view.0 + shift).clamp(0.0, duration - len);
                view = (start, start + len);
            }
        }
        self.view = if view.1 - view.0 >= duration - 1e-6 { None } else { Some(view) };

        let span = (view.1 - view.0).max(1e-6);
        let x_of = |t: f64| track.left() + ((t - view.0) / span) as f32 * track.width();
        let t_of = |x: f32| (view.0 + ((x - track.left()) / track.width()).clamp(0.0, 1.0) as f64 * span).clamp(0.0, duration);

        let Some(player) = self.player.as_mut() else { return };
        let item = &mut self.items[index];
        let editable = item.status != Status::Encoding && !sample;

        // Dragging: handles move the trim points, anywhere else scrubs.
        let near = |x: f32, t: f64| (x - x_of(t)).abs() <= HANDLE_GRAB;
        if let Some(hover) = response.hover_pos() {
            if editable && (near(hover.x, item.start) || near(hover.x, item.end)) {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
            }
        }
        if response.drag_started() {
            if let Some(ptr) = response.interact_pointer_pos() {
                let ds = (ptr.x - x_of(item.start)).abs();
                let de = (ptr.x - x_of(item.end)).abs();
                let target = if editable && ds <= HANDLE_GRAB && ds <= de {
                    DragTarget::Start
                } else if editable && de <= HANDLE_GRAB {
                    DragTarget::End
                } else {
                    DragTarget::Playhead
                };
                self.resume_after_drag = target == DragTarget::Playhead && player.is_playing();
                player.pause();
                self.drag = Some(target);
            }
        }
        if response.dragged() {
            if let (Some(target), Some(ptr)) = (self.drag, response.interact_pointer_pos()) {
                let t = t_of(ptr.x);
                match target {
                    DragTarget::Playhead => player.seek(t),
                    DragTarget::Start => {
                        item.start = t.min(item.end - MIN_CLIP).max(0.0);
                        player.seek(item.start);
                    }
                    DragTarget::End => {
                        item.end = t.max(item.start + MIN_CLIP).min(item.info.duration);
                        player.seek(item.end);
                    }
                }
            }
        }
        if response.drag_stopped() {
            if self.resume_after_drag {
                player.play();
            }
            self.resume_after_drag = false;
            if matches!(self.drag, Some(DragTarget::Start | DragTarget::End)) {
                self.start_buf = util::format_time_precise(item.start);
                self.end_buf = util::format_time_precise(item.end);
            }
            self.drag = None;
        } else if response.clicked() {
            if let Some(ptr) = response.interact_pointer_pos() {
                player.seek(t_of(ptr.x));
            }
        }

        // ---------------------------------------------------------- paint
        let painter = ui.painter_at(rect);
        let thumbs_rect = Rect::from_min_size(track.min, Vec2::new(track.width(), THUMB_H));
        let wave_rect = Rect::from_min_max(Pos2::new(track.left(), thumbs_rect.bottom()), track.max);
        painter.rect_filled(track, 6.0, p.input);

        // Ruler.
        let step = tick_step(span, track.width(), 70.0);
        let mut t = (view.0 / step).ceil() * step;
        while t <= view.1 + 1e-9 {
            let x = x_of(t);
            painter.vline(x, (rect.top() + RULER_H - 5.0)..=(rect.top() + RULER_H), Stroke::new(1.0, p.stroke_strong));
            let label = if step < 1.0 { util::format_time_precise(t) } else { util::format_time(t) };
            painter.text(Pos2::new(x + 3.0, rect.top()), egui::Align2::LEFT_TOP, label, FontId::proportional(10.5), p.weak);
            t += step;
        }

        // Thumbnails: each covers an equal share of the whole video.
        if !item.thumbs.is_empty() {
            let n = item.thumbs.len();
            for (k, tex) in item.thumbs.iter().enumerate() {
                let (a, b) = (duration * k as f64 / n as f64, duration * (k + 1) as f64 / n as f64);
                if b < view.0 || a > view.1 {
                    continue;
                }
                let cell = Rect::from_x_y_ranges(x_of(a)..=x_of(b), thumbs_rect.y_range());
                // Fill the cell, cropping the thumbnail's sides as needed.
                let size = tex.size_vec2();
                let cell_aspect = cell.width() / cell.height();
                let tex_aspect = size.x / size.y;
                let uv = if cell_aspect < tex_aspect {
                    let w = cell_aspect / tex_aspect;
                    Rect::from_min_max(Pos2::new(0.5 - w / 2.0, 0.0), Pos2::new(0.5 + w / 2.0, 1.0))
                } else {
                    let h = tex_aspect / cell_aspect;
                    Rect::from_min_max(Pos2::new(0.0, 0.5 - h / 2.0), Pos2::new(1.0, 0.5 + h / 2.0))
                };
                painter.with_clip_rect(thumbs_rect).image(tex.id(), cell, uv, Color32::from_white_alpha(230));
            }
        }

        // Waveform.
        if let Some(peaks) = &item.waveform {
            let n = peaks.len().max(1);
            let mid = wave_rect.center().y;
            let half = wave_rect.height() / 2.0 - 2.0;
            let color = p.weak.gamma_multiply(0.9);
            let mut x = wave_rect.left();
            while x < wave_rect.right() {
                let t = t_of(x);
                let k = ((t / duration) * n as f64).floor().clamp(0.0, (n - 1) as f64) as usize;
                let h = (peaks[k] * half).max(0.5);
                painter.vline(x, (mid - h)..=(mid + h), Stroke::new(1.0, color));
                x += 2.0;
            }
        } else if item.info.audio_tracks == 0 {
            painter.text(wave_rect.center(), egui::Align2::CENTER_CENTER, "No audio", FontId::proportional(11.0), p.weak);
        }

        // Outside the trim: darkened.
        let shade = Color32::from_black_alpha(150);
        let (xs, xe) = (x_of(item.start), x_of(item.end));
        if xs > track.left() {
            painter.rect_filled(Rect::from_x_y_ranges(track.left()..=xs.min(track.right()), track.y_range()), 6.0, shade);
        }
        if xe < track.right() {
            painter.rect_filled(Rect::from_x_y_ranges(xe.max(track.left())..=track.right(), track.y_range()), 6.0, shade);
        }

        // Cut parts: red, hatched; muted parts: blue band over the waveform.
        let red = Color32::from_rgb(0xE5, 0x4B, 0x4B);
        let blue = Color32::from_rgb(0x4B, 0x8B, 0xE5);
        let clip_x = |a: f32, b: f32| (a.max(track.left()), b.min(track.right()));
        for cut in &item.edit.cuts {
            let (a, b) = clip_x(x_of(cut.start), x_of(cut.end));
            if b <= a {
                continue;
            }
            let r = Rect::from_x_y_ranges(a..=b, track.y_range());
            painter.rect_filled(r, 0.0, red.gamma_multiply(0.35));
            let mut x = a - track.height();
            while x < b {
                painter.with_clip_rect(r).line_segment([Pos2::new(x, r.bottom()), Pos2::new(x + r.height(), r.top())], Stroke::new(1.0, red.gamma_multiply(0.8)));
                x += 8.0;
            }
            painter.rect_stroke(r, 0.0, Stroke::new(1.0, red), StrokeKind::Inside);
        }
        for m in &item.edit.mutes {
            let (a, b) = clip_x(x_of(m.start), x_of(m.end));
            if b > a {
                let r = Rect::from_x_y_ranges(a..=b, wave_rect.y_range());
                painter.rect_filled(r, 0.0, blue.gamma_multiply(0.45));
                painter.hline(a..=b, r.top() + 1.0, Stroke::new(2.0, blue));
            }
        }

        // Trim outline and handles.
        if xe > track.left() && xs < track.right() {
            let (a, b) = clip_x(xs, xe);
            painter.rect_stroke(Rect::from_x_y_ranges(a..=b, track.y_range()), 6.0, Stroke::new(2.0, p.accent), StrokeKind::Inside);
        }
        for x in [xs, xe] {
            if x < track.left() - 1.0 || x > track.right() + 1.0 {
                continue;
            }
            let handle = Rect::from_center_size(Pos2::new(x, track.center().y), Vec2::new(8.0, track.height() + 6.0));
            painter.rect_filled(handle, 3.0, if editable { p.accent } else { p.stroke_strong });
            for dy in [-5.0, 0.0, 5.0] {
                painter.hline((x - 2.0)..=(x + 2.0), track.center().y + dy, Stroke::new(1.0, p.on_accent));
            }
        }

        // A part being marked: from the mark to the playhead.
        let pos = player.position();
        if let Some((kind, from)) = self.mark {
            let color = if kind == MarkKind::Cut { red } else { blue };
            let (a, b) = clip_x(x_of(from.min(pos)), x_of(from.max(pos)));
            if b > a {
                painter.rect_filled(Rect::from_x_y_ranges(a..=b, track.y_range()), 0.0, color.gamma_multiply(0.25));
            }
            let xm = x_of(from);
            if xm >= track.left() && xm <= track.right() {
                painter.vline(xm, track.y_range(), Stroke::new(2.0, color));
            }
        }

        painter.rect_stroke(track, 6.0, Stroke::new(1.0, p.stroke), StrokeKind::Inside);

        // Playhead.
        if !sample {
            let xp = x_of(pos);
            if xp >= track.left() - 1.0 && xp <= track.right() + 1.0 {
                painter.vline(xp, (track.top() - 4.0)..=track.bottom(), Stroke::new(2.0, p.text));
                painter.circle_filled(Pos2::new(xp, track.top() - 4.0), 4.0, p.text);
            }
        }

        // Time under the pointer.
        if let Some(hover) = response.hover_pos() {
            let label = util::format_time_precise(t_of(hover.x));
            let galley = painter.layout_no_wrap(label, FontId::proportional(12.0), p.text);
            let pos = Pos2::new(
                (hover.x - galley.size().x / 2.0).clamp(rect.left(), rect.right() - galley.size().x),
                rect.top() - galley.size().y - 4.0,
            );
            let bg = Rect::from_min_size(pos, galley.size()).expand(3.0);
            let over = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, Id::new("timeline-hover")));
            over.rect_filled(bg, 4.0, p.surface);
            over.rect_stroke(bg, 4.0, Stroke::new(1.0, p.stroke), StrokeKind::Outside);
            over.galley(pos, galley, p.text);
        }
    }

    /// Zoom buttons: in/out around the playhead, or show everything.
    pub(super) fn zoom_by(&mut self, factor: f64) {
        let Some(index) = self.selected_index() else { return };
        let duration = self.items[index].info.duration.max(0.001);
        let view = self.view.unwrap_or((0.0, duration));
        let at = self.player.as_ref().map(|p| p.position()).unwrap_or((view.0 + view.1) / 2.0);
        let v = zoom_view(view, at, factor, duration);
        self.view = if v.1 - v.0 >= duration - 1e-6 { None } else { Some(v) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_and_zoom() {
        assert_eq!(tick_step(60.0, 600.0, 70.0), 10.0);
        assert_eq!(tick_step(3600.0, 1000.0, 70.0), 300.0);
        assert_eq!(tick_step(2.0, 1000.0, 70.0), 0.2);
        // Zooming in 2x around the middle keeps the middle in place.
        let v = zoom_view((0.0, 100.0), 50.0, 2.0, 100.0);
        assert!((v.0 - 25.0).abs() < 1e-9 && (v.1 - 75.0).abs() < 1e-9);
        // Zooming near the start stays inside the video.
        let v = zoom_view((0.0, 100.0), 1.0, 4.0, 100.0);
        assert!(v.0 >= 0.0 && (v.1 - v.0 - 25.0).abs() < 1e-9);
        // Zooming out never goes past the whole video.
        assert_eq!(zoom_view((10.0, 20.0), 15.0, 0.01, 100.0), (0.0, 100.0));
    }
}
