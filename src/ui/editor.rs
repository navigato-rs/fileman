use std::collections::{HashMap, HashSet};
use std::sync::mpsc;

use egui;

use fileman::{app_state, theme};

use crate::{HighlightRequest, color32, cursor_row_col, hash_text};

pub struct EditorRender<'a> {
    pub theme: &'a theme::Theme,
    pub is_focused: bool,
    pub edit: &'a mut app_state::EditState,
    pub highlight_cache: &'a HashMap<String, egui::text::LayoutJob>,
    pub highlight_pending: &'a mut HashSet<String>,
    pub highlight_req_tx: &'a mpsc::Sender<HighlightRequest>,
    pub available_height: f32,
}

pub fn draw_editor(ui: &mut egui::Ui, ctx: EditorRender<'_>) {
    let EditorRender {
        theme,
        is_focused,
        edit,
        highlight_cache,
        highlight_pending,
        highlight_req_tx,
        available_height,
    } = ctx;
    let colors = theme.colors();
    ui.push_id("editor_panel", |ui| {
        egui::Frame::NONE
            .fill(color32(colors.preview_bg))
            .stroke(egui::Stroke::new(
                1.0_f32,
                color32(if is_focused {
                    colors.panel_border_active
                } else {
                    colors.panel_border_inactive
                }),
            ))
            .show(ui, |ui| {
                ui.set_min_size(egui::Vec2::new(ui.available_width(), available_height));
                egui::Frame::NONE
                    .fill(color32(colors.preview_header_bg))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let title = edit
                                .path
                                .as_ref()
                                .map(|p| p.to_string_lossy().to_string())
                                .unwrap_or_else(|| "Edit".to_string());
                            ui.colored_label(
                                color32(colors.preview_header_fg),
                                format!("Edit — {title}"),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.checkbox(&mut edit.wrap, "Wrap");
                                },
                            );
                        });
                    });
                ui.add_space(2.0);
                if edit.load == app_state::EditLoad::Loading {
                    let t = ui.ctx().input(|i| i.time);
                    let dots = ".".repeat(((t * 2.0) as usize % 4) + 1);
                    ui.colored_label(color32(colors.row_fg_inactive), format!("Loading{dots}"));
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(300));
                    return;
                }
                let mut text = std::mem::take(&mut edit.text);
                let text_len_before = text.len();
                let edit_ext = edit.ext.clone();
                let theme_kind = theme.kind;
                let mut key = edit.highlight_key.clone();
                if key.is_none()
                    && let Some(path) = edit.path.as_ref()
                {
                    key = Some(format!("edit:{}", path.to_string_lossy()));
                    edit.highlight_key = key.clone();
                }
                let key_with_hash = key
                    .as_ref()
                    .map(|base| format!("{base}:{}", edit.highlight_hash));
                if let Some(key) = key_with_hash.as_ref() {
                    let cached = highlight_cache.contains_key(key);
                    let ready = edit
                        .highlight_dirty_at
                        .map(|t| t.elapsed().as_millis() > 250)
                        .unwrap_or(true);
                    if !cached && !highlight_pending.contains(key) && !text.is_empty() && ready {
                        edit.highlight_wrap_width = ui.available_width();
                        highlight_pending.insert(key.clone());
                        edit.highlight_dirty_at = None;
                        let _ = highlight_req_tx.send(HighlightRequest {
                            key: key.clone(),
                            text: text.clone(),
                            ext: edit_ext.clone(),
                            theme_kind,
                        });
                    }
                }
                let mut needs_highlight = false;
                let editor_wrap = edit.wrap;
                let mut layouter = |ui: &egui::Ui,
                                    string: &dyn egui::TextBuffer,
                                    wrap_width: f32| {
                    let effective_wrap = if editor_wrap {
                        wrap_width
                    } else {
                        f32::INFINITY
                    };
                    if let Some(key) = key_with_hash.as_ref() {
                        let _wrap_changed = (edit.highlight_wrap_width - wrap_width).abs() > 0.5;
                        if let Some(cached) = highlight_cache.get(key) {
                            let mut job = cached.clone();
                            job.wrap.max_width = effective_wrap;
                            job.wrap.break_anywhere = editor_wrap;
                            return ui.fonts_mut(|f| f.layout_job(job));
                        }
                        needs_highlight = true;
                    }
                    let mut job = egui::text::LayoutJob::simple(
                        string.as_str().to_string(),
                        egui::TextStyle::Monospace.resolve(ui.style()),
                        egui::Color32::LIGHT_GRAY,
                        effective_wrap,
                    );
                    job.wrap.break_anywhere = editor_wrap;
                    ui.fonts_mut(|f| f.layout_job(job))
                };
                let footer_height = ui.text_style_height(&egui::TextStyle::Body).max(1.0) + 8.0;
                let editor_height = (ui.available_height() - footer_height).max(0.0);
                let row_height = ui.text_style_height(&egui::TextStyle::Monospace).max(1.0);
                let desired_rows = (editor_height / row_height).floor().max(1.0) as usize;
                let mut edit_output: Option<egui::text_edit::TextEditOutput> = None;
                let scroll = if editor_wrap {
                    egui::ScrollArea::vertical()
                } else {
                    egui::ScrollArea::both()
                };
                scroll
                    .id_salt("editor_scroll")
                    .auto_shrink([false, false])
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                    .max_height(editor_height)
                    .show(ui, |ui| {
                        let mut te = egui::TextEdit::multiline(&mut text)
                            .font(egui::TextStyle::Monospace)
                            .layouter(&mut layouter)
                            .cursor_at_end(false)
                            .id_source(("editor_text", edit.request_id))
                            .desired_rows(desired_rows)
                            .lock_focus(true);
                        if editor_wrap {
                            te = te.desired_width(f32::INFINITY);
                        } else {
                            te = te.code_editor();
                        }
                        te = te
                            .frame(egui::Frame::NONE)
                            .background_color(egui::Color32::TRANSPARENT);
                        let mut output = te.show(ui);
                        // Apply forced selection for testing
                        if let Some((start, end)) = edit.force_select {
                            output.cursor_range = Some(egui::text::CCursorRange::two(
                                egui::text::CCursor::new(start),
                                egui::text::CCursor::new(end),
                            ));
                        }
                        paint_whitespace_in_selection(ui, &output, &text);
                        edit_output = Some(output);
                    });
                let response = edit_output
                    .as_ref()
                    .map(|output| output.response.clone())
                    .unwrap_or_else(|| egui::AtomLayoutResponse::empty(ui.label(" ")));
                // Auto-indent: if Enter was just pressed, copy leading whitespace
                // from the previous line. Only when text grew (not on deletion).
                if response.changed() && text.len() > text_len_before {
                    let cursor_char = edit_output
                        .as_ref()
                        .and_then(|o| o.cursor_range)
                        .map(|r| r.primary.index);
                    if let Some(char_pos) = cursor_char
                        && char_pos > 0
                    {
                        // Convert character offset to byte offset
                        let byte_pos: usize = text
                            .char_indices()
                            .nth(char_pos)
                            .map(|(i, _)| i)
                            .unwrap_or(text.len());
                        if byte_pos > 0 && text.as_bytes().get(byte_pos - 1) == Some(&b'\n') {
                            let before = &text[..byte_pos - 1];
                            let prev_line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
                            let prev_line = &before[prev_line_start..];
                            let indent: String = prev_line
                                .chars()
                                .take_while(|c| *c == ' ' || *c == '\t')
                                .collect();
                            if !indent.is_empty() {
                                let indent_chars = indent.chars().count();
                                text.insert_str(byte_pos, &indent);
                                // Update egui's cursor state to account for the
                                // inserted indent.
                                let te_id = edit_output.as_ref().unwrap().response.id;
                                if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), te_id)
                                {
                                    let new_pos = char_pos + indent_chars;
                                    state.cursor.set_char_range(Some(
                                        egui::text::CCursorRange::one(egui::text::CCursor::new(
                                            new_pos,
                                        )),
                                    ));
                                    state.store(ui.ctx(), te_id);
                                }
                            }
                        }
                    }
                }
                edit.text = text;
                if response.changed() {
                    edit.highlight_hash = hash_text(&edit.text);
                    edit.highlight_wrap_width = 0.0;
                    edit.highlight_dirty_at = Some(std::time::Instant::now());
                    edit.dirty = true;
                }
                if needs_highlight && let Some(key) = edit.highlight_key.clone() {
                    let wrap_width = ui.available_width();
                    if !highlight_pending.contains(&key) {
                        edit.highlight_wrap_width = wrap_width;
                        highlight_pending.insert(key.clone());
                        let _ = highlight_req_tx.send(HighlightRequest {
                            key,
                            text: edit.text.clone(),
                            ext: edit_ext,
                            theme_kind,
                        });
                    }
                }
                if is_focused {
                    response.request_focus();
                    ui.memory_mut(|mem| mem.request_focus(response.id));
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(500));
                }
                ui.horizontal(|ui| {
                    ui.colored_label(
                        color32(colors.row_fg_inactive),
                        "Ctrl+S: save  •  Esc: close",
                    );
                    ui.add_space(6.0);
                    let (row, col) = edit_output
                        .as_ref()
                        .and_then(|output| output.cursor_range)
                        .map(|range| range.primary.index)
                        .map(|idx| cursor_row_col(&edit.text, idx))
                        .unwrap_or((1, 1));
                    let info = format!("row: {row}, col: {col}");
                    let font = egui::TextStyle::Body.resolve(ui.style());
                    let width = ui
                        .painter()
                        .layout_no_wrap(info.clone(), font.clone(), color32(colors.row_fg_inactive))
                        .size()
                        .x;
                    let rect = ui.available_rect_before_wrap();
                    let pos = egui::pos2(rect.right() - width, rect.center().y);
                    ui.painter().text(
                        pos,
                        egui::Align2::LEFT_CENTER,
                        info,
                        font,
                        color32(colors.row_fg_inactive),
                    );
                });
            });
    });
}

/// Paint whitespace markers (· for spaces, → for tabs, ↵ for newlines) over selected text.
fn paint_whitespace_in_selection(
    ui: &egui::Ui,
    output: &egui::text_edit::TextEditOutput,
    text: &str,
) {
    let Some(range) = output.cursor_range else {
        return;
    };
    let sel_start = range.primary.index.min(range.secondary.index);
    let sel_end = range.primary.index.max(range.secondary.index);
    if sel_start == sel_end {
        return;
    }

    let galley = &output.galley;
    let galley_pos = output.galley_pos;
    let color = egui::Color32::from_white_alpha(160);
    let font = egui::FontId::monospace(11.0);

    for (char_idx, ch) in text.chars().enumerate() {
        if char_idx >= sel_end {
            break;
        }
        if char_idx < sel_start {
            continue;
        }
        let (marker, align) = match ch {
            ' ' => ("·", egui::Align2::CENTER_CENTER),
            '\t' => ("→", egui::Align2::CENTER_CENTER),
            '\n' => ("↵", egui::Align2::LEFT_CENTER),
            _ => continue,
        };
        let cursor_rect = galley.pos_from_cursor(egui::text::CCursor::new(char_idx));
        let pos = match ch {
            '\n' => {
                // Place after the last character on this line
                galley_pos + egui::vec2(cursor_rect.min.x + 2.0, cursor_rect.center().y)
            }
            _ => {
                let next = galley.pos_from_cursor(egui::text::CCursor::new(char_idx + 1));
                let cx = (cursor_rect.min.x + next.min.x) * 0.5;
                let cy = cursor_rect.center().y;
                galley_pos + egui::vec2(cx, cy)
            }
        };
        ui.painter().text(pos, align, marker, font.clone(), color);
    }
}
