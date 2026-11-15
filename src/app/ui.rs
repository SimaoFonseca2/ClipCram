//! Drawing the window. The top bar holds the size limit and settings; the queue is on the
//! left; the preview, transport and timeline in the middle; the selected video's settings
//! on the right in four tabs (Edit, Encode, Audio, Output); progress and the compress
//! button along the bottom.

use super::*;
use crate::config::ThemePref;
use crate::edit::{Crop, TextPos, TextSize, SPEEDS};
use crate::encode::{Denoise, OutputFormat};
use eframe::egui::{
    Align, ComboBox, CornerRadius, DragValue, Frame, Layout, Margin, Pos2, Rect, Sense, Slider, TextEdit, Ui, Vec2,
};
use eframe::epaint::text::{LayoutJob, TextFormat, TextWrapping};

const PANEL_GAP: i8 = 10;

/// A rounded card with a title, used to group settings.
fn card<R>(ui: &mut Ui, p: &Palette, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.stroke))
        .corner_radius(10.0)
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !title.is_empty() {
                ui.label(RichText::new(title).strong().size(13.5).color(p.text));
                ui.add_space(2.0);
            }
            add(ui)
        })
        .inner
}

fn hint(ui: &mut Ui, p: &Palette, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).size(12.0).color(p.weak));
}

/// A small rounded label with a colored background.
fn pill(ui: &mut Ui, text: &str, fg: Color32, bg: Color32) -> egui::Response {
    Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| ui.label(RichText::new(text).size(11.5).color(fg)))
        .response
}

fn accent_button(p: &Palette, text: &str) -> Button<'static> {
    Button::new(RichText::new(text.to_string()).strong().color(p.on_accent)).fill(p.accent).stroke(Stroke::NONE)
}

/// Two-column grid rows: label on the left, control on the right.
fn row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.label(label);
    add(ui);
    ui.end_row();
}

fn grid(id: &str) -> egui::Grid {
    egui::Grid::new(id).num_columns(2).spacing([12.0, 8.0]).min_col_width(90.0)
}

/// A text field that fills the row, with buttons at its right end. Returns (field lost
/// focus, index of the clicked button).
fn field_row(ui: &mut Ui, buf: &mut String, placeholder: &str, buttons: &[&str], password: bool) -> (bool, Option<usize>) {
    let mut clicked = None;
    let mut done = false;
    let size = Vec2::new(ui.available_width(), ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(size, Layout::right_to_left(Align::Center), |ui| {
        for (k, label) in buttons.iter().enumerate().rev() {
            if ui.button(*label).clicked() {
                clicked = Some(k);
            }
        }
        done = ui.add(TextEdit::singleline(buf).hint_text(placeholder).password(password).desired_width(ui.available_width())).lost_focus();
    });
    (done, clicked)
}

fn time_field(ui: &mut Ui, buf: &mut String) -> Option<f64> {
    let r = ui.add(TextEdit::singleline(buf).desired_width(84.0).font(egui::TextStyle::Monospace));
    if r.lost_focus() {
        return util::parse_time(buf);
    }
    None
}

impl App {
    pub(super) fn layout(&mut self, ctx: &egui::Context) {
        let p = self.palette();
        egui::TopBottomPanel::top("top")
            .frame(Frame::new().fill(p.surface).inner_margin(Margin { left: 16, right: 16, top: 10, bottom: 10 }).stroke(Stroke::new(1.0, p.stroke)))
            .show(ctx, |ui| self.top_bar(ui));
        egui::TopBottomPanel::bottom("bottom")
            .frame(Frame::new().fill(p.surface).inner_margin(Margin { left: 16, right: 16, top: 10, bottom: 12 }).stroke(Stroke::new(1.0, p.stroke)))
            .show(ctx, |ui| self.bottom_bar(ui));
        egui::SidePanel::left("queue")
            .resizable(true)
            .default_width(270.0)
            .width_range(220.0..=480.0)
            .frame(Frame::new().fill(p.base).inner_margin(Margin { left: 12, right: PANEL_GAP, top: 12, bottom: 12 }))
            .show(ctx, |ui| self.queue_panel(ui));
        if self.selected_index().is_some() {
            egui::SidePanel::right("inspector")
                .resizable(true)
                .default_width(360.0)
                .width_range(320.0..=600.0)
                .frame(Frame::new().fill(p.base).inner_margin(Margin { left: PANEL_GAP, right: 12, top: 12, bottom: 12 }))
                .show(ctx, |ui| self.inspector(ui));
        }
        egui::CentralPanel::default()
            .frame(Frame::new().fill(p.base).inner_margin(Margin { left: 4, right: 4, top: 12, bottom: 12 }))
            .show(ctx, |ui| {
                self.view_height = ui.available_height();
                self.center(ui);
            });
        if self.settings_open {
            self.settings_window(ctx);
        }
    }

    // ========================================================
    // TOP BAR
    // ========================================================

    fn top_bar(&mut self, ui: &mut Ui) {
        let p = self.palette();
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
            ui.painter().rect_filled(r, 6.0, p.accent);
            ui.painter().text(r.center(), Align2::CENTER_CENTER, "C", FontId::proportional(14.0), p.on_accent);
            ui.label(RichText::new("ClipCram").size(18.0).strong().color(p.text));
            ui.add_space(18.0);
            ui.label(RichText::new("Size limit").color(p.weak));
            ComboBox::from_id_salt("size-preset").selected_text(self.persisted.size_preset.label()).width(230.0).show_ui(ui, |ui| {
                for preset in SizePreset::ALL {
                    ui.selectable_value(&mut self.persisted.size_preset, preset, preset.label());
                }
            });
            if self.persisted.size_preset == SizePreset::Custom {
                ui.add(DragValue::new(&mut self.persisted.custom_mib).range(0.5..=4000.0).speed(0.1).fixed_decimals(1).suffix(" MiB"));
            }
            hint(ui, &p, format!("aiming for {:.1} MiB per video", self.persisted.target_mib()));

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("⚙  Settings").clicked() {
                    self.settings_open = !self.settings_open;
                }
                if let Some(tag) = self.update_available.clone() {
                    let url = format!("{}/releases/latest", env!("CARGO_PKG_REPOSITORY"));
                    if ui.add(Button::new(RichText::new(format!("⬆ Update {tag}")).color(p.on_accent)).fill(p.ok)).on_hover_text("A newer version is on GitHub").clicked() {
                        let _ = open::that_detached(url);
                    }
                }
                if let Some(w) = &self.watcher {
                    pill(ui, &format!("👁 Watching {}", util::file_name_lossy(&w.dir)), p.text, p.button);
                }
            });
        });
    }

    // ========================================================
    // QUEUE
    // ========================================================

    fn queue_panel(&mut self, ui: &mut Ui) {
        let p = self.palette();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Queue").strong().size(15.0).color(p.text));
            if !self.items.is_empty() {
                pill(ui, &self.items.len().to_string(), p.weak, p.button);
            }
            if !self.probing.is_empty() {
                ui.spinner();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(accent_button(&p, "+ Add videos")).clicked() {
                    self.choose_files();
                }
            });
        });
        ui.add_space(6.0);

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.horizontal(|ui| {
                let can_edit = !self.processing;
                if ui.add_enabled(can_edit && self.selected_index().is_some(), Button::new("Remove")).clicked() {
                    self.remove_selected();
                }
                if ui.add_enabled(can_edit && !self.items.is_empty(), Button::new("Clear")).clicked() {
                    self.clear_queue();
                }
            });
            ui.add_space(6.0);
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    if self.items.is_empty() {
                        ui.add_space(30.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("🎞").size(34.0).color(p.weak));
                            ui.label(RichText::new("No videos yet").strong().color(p.text));
                            hint(ui, &p, "Drop videos or a folder anywhere in this window.");
                        });
                        return;
                    }
                    let mut clicked = None;
                    let mut remove = None;
                    let mut reveal: Option<PathBuf> = None;
                    let mut copy: Option<PathBuf> = None;
                    for item in &self.items {
                        let selected = Some(item.id) == self.selected;
                        let response = queue_card(ui, &p, item, selected, self.progress);
                        if response.clicked() {
                            clicked = Some(item.id);
                        }
                        let response = match &item.error {
                            Some(e) => response.on_hover_text(e),
                            None => response,
                        };
                        response.context_menu(|ui| {
                            if ui.button("Show source file").clicked() {
                                reveal = Some(item.path.clone());
                                ui.close();
                            }
                            if let Some(out) = &item.last_output {
                                if ui.button("Show encoded file").clicked() {
                                    reveal = Some(out.clone());
                                    ui.close();
                                }
                                if ui.button("Copy encoded file to clipboard").clicked() {
                                    copy = Some(out.clone());
                                    ui.close();
                                }
                            }
                            if ui.add_enabled(!self.processing, Button::new("Remove")).clicked() {
                                remove = Some(item.id);
                                ui.close();
                            }
                        });
                        ui.add_space(6.0);
                    }
                    if let Some(id) = clicked {
                        self.select(Some(id));
                    }
                    if let Some(id) = remove {
                        self.select(Some(id));
                        self.remove_selected();
                    }
                    if let Some(path) = reveal {
                        reveal_in_folder(&path);
                    }
                    if let Some(path) = copy {
                        self.copy_to_clipboard(&path);
                    }
                });
            });
        });
    }

    // ========================================================
    // CENTER: PREVIEW, TRANSPORT, TIMELINE
    // ========================================================

    fn center(&mut self, ui: &mut Ui) {
        let p = self.palette();
        let Some(index) = self.selected_index() else {
            let rect = ui.available_rect_before_wrap();
            let painter = ui.painter_at(rect);
            let inner = rect.shrink(20.0);
            painter.rect_filled(inner, 16.0, p.surface);
            painter.rect_stroke(inner, 16.0, Stroke::new(1.5, p.stroke_strong), StrokeKind::Inside);
            painter.text(inner.center() - Vec2::new(0.0, 22.0), Align2::CENTER_CENTER, "Drop videos here", FontId::proportional(22.0), p.text);
            painter.text(
                inner.center() + Vec2::new(0.0, 10.0),
                Align2::CENTER_CENTER,
                "Trim, crop, cut and squeeze them under your size limit.",
                FontId::proportional(14.0),
                p.weak,
            );
            ui.allocate_rect(rect, Sense::hover());
            return;
        };

        // Preview takes what's left after the controls below it.
        let below = 46.0 + 96.0 + 40.0 + 24.0;
        let height = (ui.available_height() - below).max(180.0);
        self.preview(ui, index, height);
        ui.add_space(6.0);
        self.transport(ui, index);
        ui.add_space(14.0);
        self.timeline(ui);
        ui.add_space(4.0);
        self.timeline_tools(ui);
    }

    fn preview(&mut self, ui: &mut Ui, index: usize, height: f32) {
        let p = self.palette();
        let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 12.0, p.video_bg);
        if self.compare.is_some() && self.showing_sample.is_none() {
            self.compare_view(ui, rect, &response);
            return;
        }

        let Some(texture) = self.player.as_ref().and_then(|pl| pl.texture()) else {
            painter.text(rect.center(), Align2::CENTER_CENTER, "Loading preview…", FontId::proportional(15.0), p.weak);
            return;
        };
        let size = texture.size_vec2();
        let scale = (rect.width() / size.x).min(rect.height() / size.y);
        let image = Rect::from_center_size(rect.center(), size * scale);
        painter.image(texture.id(), image, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);

        if self.showing_sample.is_some() {
            let tag = Rect::from_min_size(image.min + Vec2::new(10.0, 10.0), Vec2::new(150.0, 24.0));
            painter.rect_filled(tag, 12.0, p.accent);
            painter.text(tag.center(), Align2::CENTER_CENTER, "Encoded sample", FontId::proportional(13.0), p.on_accent);
            if response.clicked() {
                if let Some(pl) = &mut self.player {
                    pl.toggle();
                }
            }
            return;
        }

        // Crop box, edited right on the picture.
        let (sw, sh) = (self.items[index].info.width, self.items[index].info.height);
        let crop = self.items[index].edit.crop.map(|c| c.clamped(sw, sh));
        let editing_crop = crop.is_some() && self.tab == Tab::Edit && self.items[index].status != Status::Encoding;
        if let Some(c) = crop {
            let k = image.width() / sw as f32;
            let box_rect = Rect::from_min_size(image.min + Vec2::new(c.x as f32 * k, c.y as f32 * k), Vec2::new(c.w as f32 * k, c.h as f32 * k));
            let dim = Color32::from_black_alpha(150);
            for r in [
                Rect::from_min_max(image.min, Pos2::new(image.right(), box_rect.top())),
                Rect::from_min_max(Pos2::new(image.left(), box_rect.bottom()), image.max),
                Rect::from_min_max(Pos2::new(image.left(), box_rect.top()), Pos2::new(box_rect.left(), box_rect.bottom())),
                Rect::from_min_max(Pos2::new(box_rect.right(), box_rect.top()), Pos2::new(image.right(), box_rect.bottom())),
            ] {
                painter.rect_filled(r, 0.0, dim);
            }
            painter.rect_stroke(box_rect, 0.0, Stroke::new(2.0, p.accent), StrokeKind::Outside);
            // Rule of thirds.
            for f in [1.0 / 3.0, 2.0 / 3.0] {
                painter.vline(box_rect.left() + box_rect.width() * f, box_rect.y_range(), Stroke::new(1.0, Color32::from_white_alpha(50)));
                painter.hline(box_rect.x_range(), box_rect.top() + box_rect.height() * f, Stroke::new(1.0, Color32::from_white_alpha(50)));
            }
            if editing_crop {
                let corners = [(false, false), (true, false), (false, true), (true, true)];
                for (right, bottom) in corners {
                    let at = Pos2::new(if right { box_rect.right() } else { box_rect.left() }, if bottom { box_rect.bottom() } else { box_rect.top() });
                    painter.rect_filled(Rect::from_center_size(at, Vec2::splat(12.0)), 3.0, p.accent);
                }
                let hit_corner = |pos: Pos2| {
                    corners.into_iter().find(|&(r, b)| {
                        let at = Pos2::new(if r { box_rect.right() } else { box_rect.left() }, if b { box_rect.bottom() } else { box_rect.top() });
                        at.distance(pos) < 14.0
                    })
                };
                if let Some(h) = response.hover_pos() {
                    if hit_corner(h).is_some() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeNwSe);
                    } else if box_rect.contains(h) {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                    }
                }
                if response.drag_started() {
                    if let Some(pos) = response.interact_pointer_pos() {
                        self.crop_drag = match hit_corner(pos) {
                            Some((r, b)) => Some((CropHandle::Corner(r, b), pos, c)),
                            None if box_rect.contains(pos) => Some((CropHandle::Move, pos, c)),
                            None => None,
                        };
                    }
                }
                if let (Some((handle, from, orig)), Some(pos)) = (self.crop_drag, response.interact_pointer_pos()) {
                    if response.dragged() {
                        let d = (pos - from) / k;
                        let (dx, dy) = (d.x.round() as i64, d.y.round() as i64);
                        let clamp = |v: i64, lo: i64, hi: i64| v.clamp(lo, hi.max(lo));
                        let (x0, y0, x1, y1) = (orig.x as i64, orig.y as i64, (orig.x + orig.w) as i64, (orig.y + orig.h) as i64);
                        let new = match handle {
                            CropHandle::Move => Crop {
                                x: clamp(x0 + dx, 0, sw as i64 - orig.w as i64) as u32,
                                y: clamp(y0 + dy, 0, sh as i64 - orig.h as i64) as u32,
                                ..orig
                            },
                            CropHandle::Corner(right, bottom) => {
                                let (mut nx0, mut ny0, mut nx1, mut ny1) = (x0, y0, x1, y1);
                                if right { nx1 = clamp(x1 + dx, x0 + 32, sw as i64) } else { nx0 = clamp(x0 + dx, 0, x1 - 32) }
                                if bottom { ny1 = clamp(y1 + dy, y0 + 32, sh as i64) } else { ny0 = clamp(y0 + dy, 0, y1 - 32) }
                                // Keep the chosen aspect ratio, if there is one.
                                if let Some((aw, ah)) = self.crop_aspect(index) {
                                    let w = (nx1 - nx0) as f64;
                                    let h = (w * ah / aw).round() as i64;
                                    if bottom { ny1 = (ny0 + h).min(sh as i64) } else { ny0 = (ny1 - h).max(0) }
                                    let h = (ny1 - ny0) as f64;
                                    let w = (h * aw / ah).round() as i64;
                                    if right { nx1 = nx0 + w } else { nx0 = nx1 - w }
                                }
                                Crop { x: nx0.max(0) as u32, y: ny0.max(0) as u32, w: (nx1 - nx0).max(32) as u32, h: (ny1 - ny0).max(32) as u32 }
                            }
                        };
                        self.items[index].edit.crop = Some(new.clamped(sw, sh));
                    }
                }
                if response.drag_stopped() {
                    self.crop_drag = None;
                }
            }
        }
        if response.clicked() && self.crop_drag.is_none() {
            if let Some(pl) = &mut self.player {
                pl.toggle();
            }
        }
    }

    /// Original on the left of the line, the encode on the right. Dragging or clicking
    /// anywhere on the picture moves the line.
    fn compare_view(&mut self, ui: &mut Ui, rect: Rect, response: &egui::Response) {
        let p = self.palette();
        let painter = ui.painter_at(rect);
        let Some(c) = &self.compare else { return };
        let size = Vec2::new(c.width as f32, c.height as f32);
        let scale = (rect.width() / size.x).min(rect.height() / size.y);
        let image = Rect::from_center_size(rect.center(), size * scale);
        if c.before.is_none() || c.after.is_none() {
            painter.text(rect.center(), Align2::CENTER_CENTER, "Loading the comparison…", FontId::proportional(15.0), p.weak);
            return;
        }
        if let Some(pos) = response.interact_pointer_pos().filter(|_| response.dragged() || response.clicked()) {
            self.compare_split = ((pos.x - image.left()) / image.width()).clamp(0.0, 1.0);
        }
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        // Zoom (Ctrl+scroll or pinch) around the pointer; scrolling pans when zoomed in.
        let (mut zoom, mut center) = (c.zoom, c.center);
        if response.hovered() {
            let (factor, scroll) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta));
            let view = 1.0 / zoom;
            let origin = (center - Vec2::splat(view / 2.0)).clamp(Vec2::ZERO, Vec2::splat(1.0 - view));
            if (factor - 1.0).abs() > 1e-3 {
                if let Some(hover) = response.hover_pos() {
                    let at = origin + ((hover - image.min) / image.size()) * view;
                    let new_zoom = (zoom * factor).clamp(1.0, 8.0);
                    let new_view = 1.0 / new_zoom;
                    let new_origin = at - ((hover - image.min) / image.size()) * new_view;
                    center = new_origin + Vec2::splat(new_view / 2.0);
                    zoom = new_zoom;
                }
            } else if zoom > 1.0 && scroll != Vec2::ZERO {
                center -= scroll / image.size() * view;
            }
        }
        let view = 1.0 / zoom;
        center = center.clamp(Vec2::splat(view / 2.0), Vec2::splat(1.0 - view / 2.0));
        let origin = center - Vec2::splat(view / 2.0);
        if let Some(c) = &mut self.compare {
            (c.zoom, c.center) = (zoom, center);
        }
        let Some(c) = &self.compare else { return };
        let (Some(before), Some(after)) = (&c.before, &c.after) else { return };

        let split = self.compare_split;
        let x = image.left() + image.width() * split;
        let left = Rect::from_min_max(image.min, Pos2::new(x, image.bottom()));
        let right = Rect::from_min_max(Pos2::new(x, image.top()), image.max);
        let uv = |a: f32, b: f32| {
            Rect::from_min_max(Pos2::new(origin.x + a * view, origin.y), Pos2::new(origin.x + b * view, origin.y + view))
        };
        if split > 0.0 {
            painter.image(before.id(), left, uv(0.0, split), Color32::WHITE);
        }
        if split < 1.0 {
            painter.image(after.id(), right, uv(split, 1.0), Color32::WHITE);
        }
        if zoom > 1.01 {
            let galley = painter.layout_no_wrap(format!("{:.1}×", zoom), FontId::proportional(12.0), Color32::WHITE);
            let r = Align2::LEFT_BOTTOM.anchor_size(Pos2::new(image.left() + 14.0, image.bottom() - 14.0), galley.size()).expand2(Vec2::new(8.0, 4.0));
            painter.rect_filled(r, 10.0, Color32::from_black_alpha(170));
            painter.galley(r.min + Vec2::new(8.0, 4.0), galley, Color32::WHITE);
        }

        // The line and its grip.
        painter.vline(x, image.y_range(), Stroke::new(2.0, Color32::WHITE));
        let knob = Pos2::new(x, image.center().y);
        painter.circle(knob, 15.0, p.accent, Stroke::new(2.0, Color32::WHITE));
        painter.text(knob, Align2::CENTER_CENTER, "⏴⏵", FontId::proportional(12.0), p.on_accent);

        // Labels, faded out when their side is mostly hidden.
        let tag = |text: &str, at: Pos2, align: Align2, visible: f32| {
            if visible < 0.08 {
                return;
            }
            let galley = painter.layout_no_wrap(text.to_string(), FontId::proportional(12.5), Color32::WHITE);
            let r = align.anchor_size(at, galley.size()).expand2(Vec2::new(8.0, 4.0));
            painter.rect_filled(r, 10.0, Color32::from_black_alpha(170));
            painter.galley(r.min + Vec2::new(8.0, 4.0), galley, Color32::WHITE);
        };
        let top = image.top() + 14.0;
        tag("Original", Pos2::new(image.left() + 14.0, top), Align2::LEFT_TOP, split);
        tag(&format!("Encoded • {}", c.summary), Pos2::new(image.right() - 14.0, top), Align2::RIGHT_TOP, 1.0 - split);
        if c.loading.is_some() {
            painter.circle_filled(Pos2::new(image.right() - 16.0, image.bottom() - 16.0), 5.0, p.accent);
        }
    }

    /// The aspect ratio the crop keeps while resizing, from the crop menu's choice.
    fn crop_aspect(&self, index: usize) -> Option<(f64, f64)> {
        let c = self.items[index].edit.crop?;
        for (aw, ah) in [(16.0, 9.0), (9.0, 16.0), (1.0, 1.0), (4.0, 3.0), (4.0, 5.0)] {
            if ((c.w as f64 / c.h as f64) - aw / ah).abs() < 0.01 {
                return Some((aw, ah));
            }
        }
        None
    }

    /// Controls while comparing: step and scrub through the sample, play it, or leave.
    fn compare_transport(&mut self, ui: &mut Ui) {
        let p = self.palette();
        let Some(c) = &self.compare else { return };
        let (mut t, duration, frame, zoom) = (c.t, c.duration, 1.0 / c.fps, c.zoom);
        let mut seek = None;
        let mut set_zoom = None;
        let (mut play, mut close, mut resample) = (false, false, false);
        // Row 1: scrub through the sample.
        ui.horizontal(|ui| {
            if ui.add_sized([44.0, 30.0], accent_button(&p, "▶")).on_hover_text("Play the encoded sample").clicked() {
                play = true;
            }
            if ui.button("⏴").on_hover_text("One frame back").clicked() {
                seek = Some(t - frame);
            }
            let time = format!("{} / {}", util::format_time_precise(t), util::format_time_precise(duration));
            let right = 40.0 + ui.spacing().item_spacing.x * 2.0 + 110.0;
            ui.spacing_mut().slider_width = (ui.available_width() - right).max(80.0);
            if ui.add(Slider::new(&mut t, 0.0..=duration.max(0.01)).show_value(false)).changed() {
                seek = Some(t);
            }
            if ui.button("⏵").on_hover_text("One frame forward").clicked() {
                seek = Some(t + frame);
            }
            ui.label(RichText::new(time).monospace().color(p.text));
        });
        // Row 2: zoom, try again, close.
        ui.horizontal(|ui| {
            hint(ui, &p, "Zoom");
            for (z, label) in [(1.0, "Fit"), (2.0, "2×"), (4.0, "4×")] {
                if ui.selectable_label((zoom - z).abs() < 0.05, label).clicked() {
                    set_zoom = Some(z);
                }
            }
            ui.separator();
            resample = ui
                .add_enabled(self.sample.is_none(), Button::new("✨ Try again with current settings"))
                .on_hover_text("Encode a new sample with whatever you've changed since")
                .clicked();
            if self.sample.is_some() {
                ui.spinner();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                close = ui.button("× Close comparison").clicked();
            });
        });
        let status = if self.sample_status.is_empty() {
            "Drag across the picture to move the line: the original is left of it, the encode right. Ctrl+scroll zooms; scroll to pan.".to_string()
        } else {
            self.sample_status.clone()
        };
        hint(ui, &p, status);
        if let Some(t) = seek {
            self.compare_seek(t);
        }
        if let (Some(z), Some(c)) = (set_zoom, &mut self.compare) {
            c.zoom = z;
            if z == 1.0 {
                c.center = Vec2::splat(0.5);
            }
        }
        if play {
            self.play_compared_sample();
        }
        if resample {
            self.start_sample();
        }
        if close {
            self.close_compare();
        }
    }

    fn transport(&mut self, ui: &mut Ui, index: usize) {
        let p = self.palette();
        if self.compare.is_some() && self.showing_sample.is_none() {
            self.compare_transport(ui);
            return;
        }
        let comparing = self.compare.is_some();
        let mut to_compare = false;
        let (start, end, duration) = (self.items[index].start, self.items[index].end, self.items[index].info.duration);
        let sample_running = self.sample.is_some();
        let showing_sample = self.showing_sample.is_some();
        let mut want_sample = false;
        let mut back = false;
        let Some(player) = self.player.as_mut() else { return };
        ui.horizontal(|ui| {
            let label = if player.is_playing() { "⏸" } else { "▶" };
            if ui.add_sized([44.0, 30.0], accent_button(&p, label)).on_hover_text("Play or pause (Space, K)").clicked() {
                player.toggle();
            }
            if !showing_sample {
                if ui.button("Play clip").on_hover_text("Play from the start point to the end point").clicked() {
                    player.play_range(start, end);
                }
                if ui.button("⏴").on_hover_text("One frame back (comma)").clicked() {
                    player.step_frame(-1.0);
                }
                if ui.button("⏵").on_hover_text("One frame forward (period)").clicked() {
                    player.step_frame(1.0);
                }
            }
            ui.label(
                RichText::new(format!(
                    "{} / {}",
                    util::format_time_precise(player.position()),
                    if showing_sample { util::format_time_precise(player.duration()) } else { util::format_time(duration) }
                ))
                    .monospace()
                    .color(p.text),
            );
            ui.add_space(8.0);
            if showing_sample {
                if comparing {
                    to_compare = ui.button("◧ Compare frames").clicked();
                }
                back = ui.button("⟲ Back to the original").clicked();
            } else if sample_running {
                ui.spinner();
            } else {
                want_sample = ui
                    .button("✨ Preview result")
                    .on_hover_text("Encode three seconds from the middle with these settings and play them")
                    .clicked();
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if player.has_audio_device() {
                    let mut volume = player.volume();
                    if ui.add(Slider::new(&mut volume, 0.0..=1.0).show_value(false)).on_hover_text("Preview volume").changed() {
                        player.set_volume(volume);
                        self.persisted.volume = volume;
                    }
                    let muted = player.muted();
                    if ui.selectable_label(muted, if muted { "🔇" } else { "🔊" }).on_hover_text("Mute the preview").clicked() {
                        player.set_muted(!muted);
                        self.persisted.muted = !muted;
                    }
                } else {
                    ui.label(RichText::new("🔈").color(p.weak)).on_hover_text("No audio output device: the preview is silent");
                }
            });
        });
        if !self.sample_status.is_empty() {
            hint(ui, &p, self.sample_status.clone());
        }
        if want_sample {
            self.start_sample();
        }
        if to_compare {
            self.back_to_compare();
        } else if back {
            if comparing {
                self.close_compare();
            } else {
                self.back_to_source();
            }
        }
    }

    fn timeline_tools(&mut self, ui: &mut Ui) {
        let marking = self.mark.map(|(k, _)| k);
        let editable = self.selected_index().is_some_and(|i| self.items[i].status != Status::Encoding) && self.showing_sample.is_none();
        ui.horizontal(|ui| {
            ui.add_enabled_ui(editable, |ui| {
                if ui.button("Start  I").on_hover_text("Set the start at the playhead").clicked() {
                    self.set_start_here();
                }
                if ui.button("End  O").on_hover_text("Set the end at the playhead").clicked() {
                    self.set_end_here();
                }
                ui.separator();
                let cut_label = if marking == Some(MarkKind::Cut) { "✂ End cut  C" } else { "✂ Cut  C" };
                if ui.selectable_label(marking == Some(MarkKind::Cut), cut_label).on_hover_text("Press at the start of a part and again at its end to remove it").clicked() {
                    self.toggle_mark(MarkKind::Cut);
                }
                let mute_label = if marking == Some(MarkKind::Mute) { "🔇 End mute  M" } else { "🔇 Mute  M" };
                if ui.selectable_label(marking == Some(MarkKind::Mute), mute_label).on_hover_text("Press at the start of a part and again at its end to silence it").clicked() {
                    self.toggle_mark(MarkKind::Mute);
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled(self.view.is_some(), Button::new("Fit")).clicked() {
                    self.view = None;
                }
                if ui.button("+").on_hover_text("Zoom in (Ctrl+scroll)").clicked() {
                    self.zoom_by(2.0);
                }
                if ui.button("-").on_hover_text("Zoom out").clicked() {
                    self.zoom_by(0.5);
                }
            });
        });
    }

    // ========================================================
    // INSPECTOR
    // ========================================================

    fn inspector(&mut self, ui: &mut Ui) {
        let p = self.palette();
        let Some(index) = self.selected_index() else { return };
        // Segmented tab bar.
        Frame::new().fill(p.surface).stroke(Stroke::new(1.0, p.stroke)).corner_radius(10.0).inner_margin(Margin::same(4)).show(ui, |ui| {
            ui.columns(4, |cols| {
                for (col, (tab, label)) in cols.iter_mut().zip([(Tab::Edit, "Edit"), (Tab::Encode, "Encode"), (Tab::Audio, "Audio"), (Tab::Output, "Output")]) {
                    let selected = self.tab == tab;
                    let button = if selected {
                        Button::new(RichText::new(label).strong().color(p.on_accent)).fill(p.accent).stroke(Stroke::NONE)
                    } else {
                        Button::new(RichText::new(label).color(p.text)).fill(Color32::TRANSPARENT).stroke(Stroke::NONE)
                    };
                    if col.add_sized([col.available_width(), 28.0], button).clicked() {
                        self.tab = tab;
                    }
                }
            });
        });
        ui.add_space(8.0);
        let editable = self.items[index].status != Status::Encoding;
        if !editable {
            hint(ui, &p, "This video is being encoded; its settings are locked until it finishes.");
        }
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_enabled_ui(editable, |ui| match self.tab {
                Tab::Edit => self.edit_tab(ui, index),
                Tab::Encode => self.encode_tab(ui, index),
                Tab::Audio => self.audio_tab(ui, index),
                Tab::Output => self.output_tab(ui, index),
            });
        });
    }

    fn edit_tab(&mut self, ui: &mut Ui, index: usize) {
        let p = self.palette();
        let caps = self.caps.clone();
        let duration = self.items[index].info.duration;

        card(ui, &p, "Trim", |ui| {
            grid("trim").show(ui, |ui| {
                row(ui, "Start", |ui| {
                    if let Some(t) = time_field(ui, &mut self.start_buf) {
                        let item = &mut self.items[index];
                        item.start = t.clamp(0.0, (item.end - MIN_CLIP).max(0.0));
                    }
                });
                row(ui, "End", |ui| {
                    if let Some(t) = time_field(ui, &mut self.end_buf) {
                        let item = &mut self.items[index];
                        item.end = t.clamp(item.start + MIN_CLIP, duration);
                    }
                });
            });
            if ui.button("Use the whole video").clicked() {
                let item = &mut self.items[index];
                item.start = 0.0;
                item.end = duration;
            }
            let item = &self.items[index];
            let out = item.edit.output_duration(item.start, item.end);
            hint(
                ui,
                &p,
                format!(
                    "Source {} • clip {} • finished video {}",
                    util::format_time(duration),
                    util::format_time_precise(item.end - item.start),
                    util::format_time_precise(out)
                ),
            );
        });
        if self.drag.is_none() && !ui.ctx().wants_keyboard_input() {
            self.start_buf = util::format_time_precise(self.items[index].start);
            self.end_buf = util::format_time_precise(self.items[index].end);
        }
        ui.add_space(8.0);

        card(ui, &p, "Cut and muted parts", |ui| {
            let item = &mut self.items[index];
            if item.edit.cuts.is_empty() && item.edit.mutes.is_empty() {
                hint(ui, &p, "Press C at the start of a part and again at its end to cut it out; M does the same to silence it.");
            }
            let mut remove_cut = None;
            let mut remove_mute = None;
            for (k, c) in item.edit.cuts.iter().enumerate() {
                ui.horizontal(|ui| {
                    pill(ui, "✂ cut", Color32::WHITE, Color32::from_rgb(0xC2, 0x3B, 0x3B));
                    ui.label(RichText::new(format!("{} – {}", util::format_time_precise(c.start), util::format_time_precise(c.end))).monospace());
                    if ui.small_button("×").on_hover_text("Keep this part after all").clicked() {
                        remove_cut = Some(k);
                    }
                });
            }
            for (k, m) in item.edit.mutes.iter().enumerate() {
                ui.horizontal(|ui| {
                    pill(ui, "🔇 muted", Color32::WHITE, Color32::from_rgb(0x3B, 0x6E, 0xC2));
                    ui.label(RichText::new(format!("{} – {}", util::format_time_precise(m.start), util::format_time_precise(m.end))).monospace());
                    if ui.small_button("×").on_hover_text("Unmute this part").clicked() {
                        remove_mute = Some(k);
                    }
                });
            }
            if let Some(k) = remove_cut {
                item.edit.cuts.remove(k);
            }
            if let Some(k) = remove_mute {
                item.edit.mutes.remove(k);
            }
        });
        ui.add_space(8.0);

        card(ui, &p, "Crop", |ui| {
            let (sw, sh) = (self.items[index].info.width, self.items[index].info.height);
            let current = match (self.items[index].edit.crop, self.crop_aspect(index)) {
                (None, _) => "Off".to_string(),
                (Some(_), Some((aw, ah))) => format!("{aw}:{ah}"),
                (Some(_), None) => "Free".to_string(),
            };
            ComboBox::from_id_salt("crop").selected_text(&current).width(200.0).show_ui(ui, |ui| {
                let item = &mut self.items[index];
                if ui.selectable_label(current == "Off", "Off").clicked() {
                    item.edit.crop = None;
                }
                if ui.selectable_label(current == "Free", "Free").clicked() {
                    item.edit.crop = Some(Crop { x: sw / 10, y: sh / 10, w: sw * 8 / 10, h: sh * 8 / 10 }.clamped(sw, sh));
                }
                for (aw, ah, label) in [(16, 9, "16:9 (widescreen)"), (9, 16, "9:16 (vertical, phones)"), (1, 1, "1:1 (square)"), (4, 3, "4:3"), (4, 5, "4:5 (portrait post)")] {
                    if ui.selectable_label(current == format!("{aw}:{ah}"), label).clicked() {
                        item.edit.crop = Some(Crop::centered(sw, sh, aw, ah));
                    }
                }
            });
            if let Some(c) = self.items[index].edit.crop {
                hint(ui, &p, format!("{} × {} at {}, {}. Drag the box on the preview to move it, or a corner to resize it.", c.w, c.h, c.x, c.y));
            }
        });
        ui.add_space(8.0);

        card(ui, &p, "Speed", |ui| {
            let item = &mut self.items[index];
            let label = |s: f64| if (s - 1.0).abs() < 1e-9 { "Normal".to_string() } else { format!("{}×", util::trim_float(s)) };
            ComboBox::from_id_salt("speed").selected_text(label(item.edit.speed())).width(200.0).show_ui(ui, |ui| {
                for s in SPEEDS {
                    ui.selectable_value(&mut item.edit.speed, s, label(s));
                }
            });
            hint(ui, &p, "The preview plays at normal speed. Audio keeps its pitch.");
        });
        ui.add_space(8.0);

        card(ui, &p, "Caption", |ui| {
            if !caps.drawtext {
                hint(ui, &p, "This FFmpeg build can't draw text (no drawtext filter).");
                return;
            }
            let item = &mut self.items[index];
            ui.add(TextEdit::singleline(&mut item.edit.text).hint_text("Text over the video").desired_width(ui.available_width()));
            ui.horizontal(|ui| {
                ui.selectable_value(&mut item.edit.text_pos, TextPos::Top, "Top");
                ui.selectable_value(&mut item.edit.text_pos, TextPos::Bottom, "Bottom");
                ui.separator();
                ui.selectable_value(&mut item.edit.text_size, TextSize::Small, "Small");
                ui.selectable_value(&mut item.edit.text_size, TextSize::Medium, "Medium");
                ui.selectable_value(&mut item.edit.text_size, TextSize::Large, "Large");
            });
        });
    }

    fn encode_tab(&mut self, ui: &mut Ui, index: usize) {
        let p = self.palette();
        let caps = self.caps.clone();
        let mut new_settings = self.items[index].settings;
        let mut apply_all = false;
        let mut apply_preset = None;
        let mut delete_preset = None;
        let mut save_preset = false;

        card(ui, &p, "Presets", |ui| {
            if self.persisted.presets.is_empty() {
                hint(ui, &p, "Save these settings, size limit included, to reuse them with one click.");
            }
            for (k, preset) in self.persisted.presets.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.button(&preset.name).on_hover_text("Apply to this video").clicked() {
                        apply_preset = Some(k);
                    }
                    hint(ui, &p, format!("{} • {}", preset.settings.codec.family_label(), preset.size_preset.label()));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("×").on_hover_text("Delete preset").clicked() {
                            delete_preset = Some(k);
                        }
                    });
                });
            }
            ui.horizontal(|ui| {
                let r = ui.add(TextEdit::singleline(&mut self.preset_name).hint_text("Preset name, e.g. Valorant clips").desired_width(200.0));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if ui.add_enabled(!self.preset_name.trim().is_empty(), Button::new("Save")).clicked() || enter {
                    save_preset = true;
                }
            });
        });
        ui.add_space(8.0);

        card(ui, &p, "Format and encoder", |ui| {
            grid("encode").show(ui, |ui| {
                row(ui, "Format", |ui| {
                    ComboBox::from_id_salt("format").selected_text(new_settings.format.label()).width(230.0).show_ui(ui, |ui| {
                        for f in OutputFormat::ALL {
                            let ok = f != OutputFormat::Webp || caps.webp.is_some();
                            ui.add_enabled_ui(ok, |ui| ui.selectable_value(&mut new_settings.format, f, f.label()));
                        }
                    });
                });
                if new_settings.format == OutputFormat::Mp4 {
                    row(ui, "Encode on", |ui| {
                        let current = new_settings.codec.engine();
                        let mut engines = caps.engines();
                        if !engines.iter().any(|(e, _)| *e == current) {
                            engines.push((current, caps.engine_name(current)));
                        }
                        let engine_label = |engine: Engine, name: &str| match engine.backend {
                            Backend::Software => "CPU (best quality)".to_string(),
                            _ => format!("{name} (fast)"),
                        };
                        let mut chosen = current;
                        ComboBox::from_id_salt("engine").selected_text(engine_label(current, &caps.engine_name(current))).width(230.0).show_ui(ui, |ui| {
                            for (engine, name) in &engines {
                                ui.selectable_value(&mut chosen, *engine, engine_label(*engine, name));
                            }
                        });
                        if chosen != current {
                            let codecs = caps.codecs_on(chosen);
                            let same = codecs.iter().find(|c| c.family == new_settings.codec.family);
                            if let Some(&codec) = same.or(codecs.first()) {
                                new_settings.codec = codec;
                            }
                        }
                    });
                    row(ui, "Codec", |ui| {
                        let mut options = caps.codecs_on(new_settings.codec.engine());
                        if !options.contains(&new_settings.codec) {
                            options.push(new_settings.codec);
                        }
                        ComboBox::from_id_salt("codec").selected_text(new_settings.codec.family_label()).width(230.0).show_ui(ui, |ui| {
                            for codec in options {
                                ui.selectable_value(&mut new_settings.codec, codec, codec.family_label());
                            }
                        });
                    });
                    row(ui, "Quality", |ui| {
                        ComboBox::from_id_salt("speed-q").selected_text(new_settings.speed.label()).width(230.0).show_ui(ui, |ui| {
                            for s in Speed::ALL {
                                ui.selectable_value(&mut new_settings.speed, s, s.label());
                            }
                        });
                    });
                }
                row(ui, "Resolution", |ui| {
                    ComboBox::from_id_salt("resolution").selected_text(new_settings.resolution.label()).width(230.0).show_ui(ui, |ui| {
                        for mode in ResolutionMode::ALL {
                            ui.selectable_value(&mut new_settings.resolution, mode, mode.label());
                        }
                    });
                });
                row(ui, "Frame rate", |ui| {
                    ComboBox::from_id_salt("fps").selected_text(new_settings.fps.label()).width(230.0).show_ui(ui, |ui| {
                        for mode in FpsMode::ALL {
                            ui.selectable_value(&mut new_settings.fps, mode, mode.label());
                        }
                    });
                });
                row(ui, "Denoise", |ui| {
                    ui.add_enabled_ui(caps.denoise, |ui| {
                        ComboBox::from_id_salt("denoise").selected_text(new_settings.denoise.label()).width(230.0).show_ui(ui, |ui| {
                            for d in Denoise::ALL {
                                ui.selectable_value(&mut new_settings.denoise, d, d.label());
                            }
                        });
                    });
                });
            });
            if self.items[index].info.hdr.is_some() {
                ui.add_enabled_ui(caps.tonemap, |ui| ui.checkbox(&mut new_settings.hdr_to_sdr, "Convert HDR to SDR so colors look right on Discord"));
            }
            if new_settings.denoise != Denoise::Off {
                hint(ui, &p, "Denoise smooths grain and sensor noise, which leaves more bitrate for detail.");
            }
        });
        ui.add_space(8.0);

        // What the encoder will make of it.
        let item = &self.items[index];
        let out = encode::out_info_for(&item.info, item.start, item.end, &item.edit);
        if new_settings.format == OutputFormat::Mp4 && out.duration > 0.0 {
            let est = encode::estimate(&new_settings, &out, out.duration, self.persisted.target_bytes(), &caps);
            card(ui, &p, "Estimate", |ui| {
                ui.label(
                    RichText::new(format!(
                        "{} × {} at {} fps, {} kbps video + {} kbps audio",
                        est.plan.width,
                        est.plan.height,
                        encode::format_fps(est.plan.fps),
                        util::thousands(est.video_bps / 1000),
                        est.audio_bps / 1000
                    ))
                    .color(p.text),
                );
                if est.tight {
                    ui.label(RichText::new("This clip is long for the size limit, so it will look soft. Trim it, cut parts out or raise the limit.").color(p.accent_ink));
                }
                if encode::copy_eligible(&self.job_spec(index), &caps) {
                    ui.label(RichText::new("✓ Already fits and plays on Discord: it will be copied without re-encoding.").color(p.ok));
                }
                hint(ui, &p, super::codec_info(new_settings.codec, new_settings.speed, &caps));
            });
            ui.add_space(8.0);
        }
        if self.items.len() > 1 && ui.button("Apply these settings to all videos").clicked() {
            apply_all = true;
        }

        if new_settings != self.items[index].settings {
            let codec_picked = new_settings.codec != self.items[index].settings.codec;
            let stand_in = self.items[index].codec_pending;
            self.apply_settings(index, new_settings);
            let codec = if codec_picked || !stand_in { new_settings.codec } else { self.remembered.codec };
            self.remembered = EncodeSettings { codec, ..new_settings };
        }
        if apply_all {
            self.apply_to_all();
        }
        if let Some(k) = apply_preset {
            self.apply_preset(k);
        }
        if let Some(k) = delete_preset {
            self.persisted.presets.remove(k);
        }
        if save_preset {
            self.save_preset();
        }
    }

    fn audio_tab(&mut self, ui: &mut Ui, index: usize) {
        let p = self.palette();
        let caps = self.caps.clone();
        let info = self.items[index].info.clone();
        let mut new_settings = self.items[index].settings;
        let mut reload = false;
        if info.audio_tracks == 0 {
            card(ui, &p, "Audio", |ui| hint(ui, &p, "This video has no audio."));
            return;
        }
        if new_settings.format != OutputFormat::Mp4 {
            card(ui, &p, "Audio", |ui| hint(ui, &p, "GIF and WebP have no sound."));
            return;
        }
        card(ui, &p, "Audio", |ui| {
            grid("audio").show(ui, |ui| {
                row(ui, "Quality", |ui| {
                    ComboBox::from_id_salt("audio").selected_text(new_settings.audio.label()).width(200.0).show_ui(ui, |ui| {
                        for mode in AudioMode::ALL {
                            ui.selectable_value(&mut new_settings.audio, mode, mode.label());
                        }
                    });
                });
                if info.audio_tracks > 1 && new_settings.audio != AudioMode::Off {
                    row(ui, "Tracks", |ui| {
                        ComboBox::from_id_salt("tracks").selected_text(new_settings.tracks.label()).width(200.0).show_ui(ui, |ui| {
                            for mode in TrackMode::ALL {
                                ui.selectable_value(&mut new_settings.tracks, mode, mode.label());
                            }
                        });
                    });
                }
            });
            ui.add_enabled_ui(caps.loudnorm && new_settings.audio != AudioMode::Off, |ui| {
                ui.checkbox(&mut new_settings.normalize, "Even out loudness (-16 LUFS)");
            });
            hint(ui, &p, "Makes quiet clips louder and loud ones quieter, to the level most online video uses.");
        });
        if info.audio_tracks > 1 && new_settings.tracks == TrackMode::Mix && new_settings.audio != AudioMode::Off {
            ui.add_space(8.0);
            card(ui, &p, "Track volume", |ui| {
                hint(ui, &p, "Balance the tracks before they're mixed, such as game audio against your mic.");
                let item = &mut self.items[index];
                if item.edit.track_volumes.len() < info.audio_tracks {
                    item.edit.track_volumes.resize(info.audio_tracks, 1.0);
                }
                for t in 0..info.audio_tracks {
                    let title = info.audio_titles.get(t).filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| format!("Track {}", t + 1));
                    ui.horizontal(|ui| {
                        ui.add_sized([90.0, 20.0], egui::Label::new(RichText::new(title).color(p.text)).truncate());
                        let r = ui.add(Slider::new(&mut item.edit.track_volumes[t], 0.0..=2.0).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
                        if r.drag_stopped() || (r.changed() && !r.dragged()) {
                            reload = true;
                        }
                    });
                }
            });
        }
        if new_settings != self.items[index].settings {
            let tracks_changed = new_settings.tracks != self.items[index].settings.tracks;
            self.apply_settings(index, new_settings);
            self.remembered = EncodeSettings { codec: self.remembered.codec, ..new_settings };
            reload |= tracks_changed;
        }
        if reload {
            self.reload_preview();
        }
    }

    fn output_tab(&mut self, ui: &mut Ui, index: usize) {
        let p = self.palette();
        let mut browse = false;
        let mut reset = false;
        let mut folder_done = false;
        let mut name_done = false;
        let mut reveal: Option<PathBuf> = None;
        let mut copy: Option<PathBuf> = None;
        let mut post: Option<PathBuf> = None;

        card(ui, &p, "Save to", |ui| {
            ui.label("Folder");
            let (done, b) = field_row(ui, &mut self.folder_buf, "Output folder", &["Browse…"], false);
            folder_done = done;
            browse = b.is_some();
            ui.label("File name");
            let (done, b) = field_row(ui, &mut self.name_buf, "output.mp4", &["Reset"], false);
            name_done = done;
            reset = b.is_some();
            hint(ui, &p, format!("{}", self.items[index].output_path().display()));
            hint(ui, &p, "Existing files are never overwritten; a number is added instead.");
        });
        ui.add_space(8.0);

        let item = &self.items[index];
        if let Some(out) = item.last_output.clone() {
            card(ui, &p, "Last result", |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(util::file_name_lossy(&out)).strong().color(p.text));
                    if let Status::Done { size_mib } = item.status {
                        pill(ui, &format!("{size_mib:.2} MiB"), p.text, p.button);
                    }
                });
                if item.copied {
                    ui.label(RichText::new("Copied without re-encoding: same quality as the source.").color(p.ok));
                } else if let Some(q) = item.quality {
                    let color = if q >= 0.94 { p.ok } else if q >= 0.90 { p.accent_ink } else { p.bad };
                    ui.horizontal(|ui| {
                        ui.label("Quality");
                        pill(ui, encode::quality_label(q), p.on_accent, color);
                        hint(ui, &p, format!("SSIM {q:.3} against the source"));
                    });
                }
                ui.horizontal(|ui| {
                    if ui.button("📋 Copy to clipboard").on_hover_text("Then paste into Discord").clicked() {
                        copy = Some(out.clone());
                    }
                    if ui.button("📂 Show in folder").clicked() {
                        reveal = Some(out.clone());
                    }
                    if services::webhook_url_ok(&self.persisted.webhook_url) && ui.button("Post to Discord").clicked() {
                        post = Some(out.clone());
                    }
                });
            });
            ui.add_space(8.0);
        }
        if let (Status::Error, Some(err)) = (&item.status, &item.error) {
            card(ui, &p, "Last attempt failed", |ui| {
                ui.label(RichText::new(err.lines().last().unwrap_or("")).color(p.bad));
            });
        }

        if folder_done {
            self.commit_output_folder();
        }
        if name_done {
            self.commit_output_name();
        }
        if browse {
            self.choose_output_folder();
        }
        if reset {
            self.reset_output_name();
        }
        if let Some(path) = reveal {
            reveal_in_folder(&path);
        }
        if let Some(path) = copy {
            self.copy_to_clipboard(&path);
        }
        if let Some(path) = post {
            self.post_to_discord(&path);
        }
    }

    // ========================================================
    // BOTTOM BAR
    // ========================================================

    fn bottom_bar(&mut self, ui: &mut Ui) {
        let p = self.palette();
        ui.horizontal(|ui| {
            let button_w = if self.processing { 330.0 } else { 230.0 };
            ui.vertical(|ui| {
                ui.set_width(ui.available_width() - button_w - 12.0);
                ui.label(RichText::new(&self.status_text).color(p.text));
                let detail = if self.progress_detail.is_empty() { format!("{:.0}%", self.progress * 100.0) } else { self.progress_detail.clone() };
                progress_bar(ui, &p, self.progress, &detail, self.processing || self.progress > 0.0);
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.processing && ui.add_sized([90.0, 42.0], Button::new("Cancel")).clicked() {
                    self.cancel_encoding();
                }
                let label = if self.processing { "Compressing…" } else { "Compress all videos" };
                let button = Button::new(RichText::new(label).size(16.0).strong().color(p.on_accent)).fill(p.accent).stroke(Stroke::NONE);
                if ui.add_enabled_ui(!self.processing && !self.items.is_empty(), |ui| ui.add_sized([224.0, 42.0], button)).inner.clicked() {
                    self.start_queue();
                }
            });
        });
    }

    // ========================================================
    // SETTINGS WINDOW
    // ========================================================

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.settings_open;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .default_height(560.0)
            .show(ctx, |ui| {
                ScrollArea::vertical().show(ui, |ui| self.settings_ui(ui));
            });
        self.settings_open = open;
    }

    fn settings_ui(&mut self, ui: &mut Ui) {
        let p = self.palette();
        card(ui, &p, "Output", |ui| {
            ui.label("Default folder for new videos");
            let (done, b) = field_row(ui, &mut self.default_dir_buf, "Folder", &["Browse…", "Use for selected"], false);
            if done {
                self.commit_default_dir();
            }
            match b {
                Some(0) => self.choose_default_folder(),
                Some(1) if self.selected_index().is_some() => self.use_default_for_selected(),
                _ => {}
            }
            ui.checkbox(&mut self.persisted.copy_if_fits, "Copy instead of re-encoding when a clip already fits and plays on Discord");
            ui.checkbox(&mut self.persisted.measure_quality, "Measure quality after encoding (takes a little longer)");
            ui.checkbox(&mut self.persisted.copy_to_clipboard, "Copy each finished video to the clipboard, ready to paste");
            ui.checkbox(&mut self.persisted.notify_when_done, "Show a notification when the queue finishes");
            ui.checkbox(&mut self.persisted.restore_queue, "Bring back the queue, with trims and edits, next time");
        });
        ui.add_space(8.0);

        card(ui, &p, "Watch folder", |ui| {
            hint(ui, &p, "Pick up new clips as they appear, such as OBS replay-buffer or ShadowPlay saves.");
            let mut changed = false;
            let (done, b) = field_row(ui, &mut self.persisted.watch_dir, "Folder to watch", &["Browse…"], false);
            changed |= done;
            if b.is_some() {
                if let Some(dir) = rfd::FileDialog::new().set_title("Folder to watch").pick_folder() {
                    self.persisted.watch_dir = dir.to_string_lossy().into_owned();
                    changed = true;
                }
            }
            changed |= ui.checkbox(&mut self.persisted.watch_enabled, "Watch this folder").changed();
            ui.add_enabled_ui(self.persisted.watch_enabled, |ui| {
                ui.checkbox(&mut self.persisted.watch_auto_encode, "Encode new clips right away");
            });
            if changed {
                self.restart_watcher();
            }
            if self.persisted.watch_enabled && self.watcher.is_none() {
                ui.label(RichText::new("That folder doesn't exist.").color(p.bad));
            }
        });
        ui.add_space(8.0);

        card(ui, &p, "Discord webhook", |ui| {
            hint(ui, &p, "Post finished videos straight to a channel. In Discord: Server Settings > Integrations > Webhooks > Copy Webhook URL. Keep it private: anyone with it can post there.");
            field_row(ui, &mut self.persisted.webhook_url, "https://discord.com/api/webhooks/…", &[], true);
            let ok = services::webhook_url_ok(&self.persisted.webhook_url);
            if !self.persisted.webhook_url.trim().is_empty() && !ok {
                ui.label(RichText::new("The address must start with https://").color(p.bad));
            }
            ui.add_enabled_ui(ok, |ui| ui.checkbox(&mut self.persisted.webhook_enabled, "Post every finished video"));
        });
        ui.add_space(8.0);

        card(ui, &p, "File manager", |ui| {
            hint(ui, &p, if cfg!(windows) {
                "Add \"Squeeze with ClipCram\" to Explorer's right-click menu. It encodes with the settings saved here, without opening the window."
            } else {
                "Add \"Squeeze with ClipCram\" to the right-click menu of Dolphin, Nautilus and Nemo. It encodes with the settings saved here, without opening the window."
            });
            ui.horizontal(|ui| {
                let label = if self.menu_installed { "Update menu entry" } else { "Add menu entry" };
                if ui.button(label).clicked() {
                    match services::install_menu() {
                        Ok(msg) => self.status_text = msg,
                        Err(e) => self.warn("Couldn't add the menu entry", e),
                    }
                    self.menu_installed = services::menu_installed();
                }
                if self.menu_installed && ui.button("Remove").clicked() {
                    if let Ok(msg) = services::uninstall_menu() {
                        self.status_text = msg;
                    }
                    self.menu_installed = services::menu_installed();
                }
            });
        });
        ui.add_space(8.0);

        card(ui, &p, "App", |ui| {
            ui.horizontal(|ui| {
                ui.label("Theme");
                let before = self.persisted.theme;
                ui.selectable_value(&mut self.persisted.theme, ThemePref::System, "System");
                ui.selectable_value(&mut self.persisted.theme, ThemePref::Dark, "Dark");
                ui.selectable_value(&mut self.persisted.theme, ThemePref::Light, "Light");
                if before != self.persisted.theme {
                    theme::set_preference(ui.ctx(), self.persisted.theme);
                }
            });
            ui.checkbox(&mut self.persisted.check_updates, "Check GitHub for new versions at startup");
            hint(
                ui,
                &p,
                format!(
                    "ClipCram {} • {} • GPU encoders: {}",
                    env!("CARGO_PKG_VERSION"),
                    self.tools.as_ref().map(|t| t.ffmpeg.display().to_string()).unwrap_or_default(),
                    if self.caps.gpu.is_empty() { "none found".to_string() } else { self.caps.gpu.iter().map(|g| g.encoder.as_str()).collect::<Vec<_>>().join(", ") }
                ),
            );
        });
    }
}

fn progress_bar(ui: &mut Ui, p: &Palette, fraction: f32, text: &str, show_text: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 9.0, p.input);
    let fraction = fraction.clamp(0.0, 1.0);
    if fraction > 0.0 {
        let fill = Rect::from_min_size(rect.min, Vec2::new((rect.width() * fraction).max(18.0), rect.height()));
        painter.rect_filled(fill, 9.0, p.accent);
    }
    if show_text {
        let galley = painter.layout_no_wrap(text.to_string(), FontId::proportional(12.0), p.text);
        let pos = rect.center() - galley.size() / 2.0;
        let bg = Rect::from_min_size(pos, galley.size()).expand2(Vec2::new(6.0, 1.0));
        painter.rect_filled(bg, 4.0, p.surface.gamma_multiply(0.85));
        painter.galley(pos, galley, p.text);
    }
}

/// One video in the queue: thumbnail, name, status, and a progress line while encoding.
fn queue_card(ui: &mut Ui, p: &Palette, item: &Item, selected: bool, progress: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 72.0), Sense::click());
    let painter = ui.painter_at(rect);
    let bg = if selected { p.button_hover } else if response.hovered() { p.button } else { p.surface };
    painter.rect(rect, 10.0, bg, Stroke::new(if selected { 1.5 } else { 1.0 }, if selected { p.accent } else { p.stroke }), StrokeKind::Inside);

    let thumb = Rect::from_min_size(rect.min + Vec2::new(8.0, 8.0), Vec2::new(96.0, 56.0));
    painter.rect_filled(thumb, 6.0, p.video_bg);
    if let Some(tex) = item.thumbs.get(item.thumbs.len() / 3) {
        let size = tex.size_vec2();
        let scale = (thumb.width() / size.x).max(thumb.height() / size.y);
        let shown = Vec2::new(thumb.width() / (size.x * scale), thumb.height() / (size.y * scale));
        let uv = Rect::from_center_size(Pos2::new(0.5, 0.5), shown);
        painter.image(tex.id(), thumb, uv, Color32::WHITE);
    }
    let left = thumb.right() + 10.0;
    let max_w = rect.right() - left - 10.0;
    let text = |s: &str, size: f32, color: Color32| {
        let mut job = LayoutJob::default();
        job.append(s, 0.0, TextFormat { font_id: FontId::proportional(size), color, ..Default::default() });
        job.wrap = TextWrapping::truncate_at_width(max_w);
        painter.layout_job(job)
    };
    painter.galley(Pos2::new(left, rect.top() + 9.0), text(&item.name(), 14.0, p.text), p.text);
    let out = item.edit.output_duration(item.start, item.end);
    let mut meta = format!("{} • {}", util::format_time(out), item.settings.codec.short());
    if item.settings.format != OutputFormat::Mp4 {
        meta = format!("{} • {}", util::format_time(out), item.settings.format.ext().to_uppercase());
    }
    if !item.edit.cuts.is_empty() {
        meta.push_str(" • ✂");
    }
    painter.galley(Pos2::new(left, rect.top() + 29.0), text(&meta, 12.0, p.weak), p.weak);
    let status = match &item.status {
        Status::Encoding => format!("Encoding {:.0}%", progress * 100.0),
        s => s.text(),
    };
    painter.galley(Pos2::new(left, rect.top() + 47.0), text(&status, 12.0, item.status.color(p)), p.text);

    if item.status == Status::Encoding {
        let bar = Rect::from_min_size(Pos2::new(rect.left() + 8.0, rect.bottom() - 5.0), Vec2::new((rect.width() - 16.0) * progress.clamp(0.0, 1.0), 3.0));
        painter.rect_filled(bar, 1.5, p.accent);
    }
    response
}
