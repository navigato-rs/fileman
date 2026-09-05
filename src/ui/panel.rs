use std::sync::mpsc;

use fileman::{app_state, archive, core, theme};

use crate::input::open_selected;
use crate::{
    DATE_COL_WIDTH, ImageCache, ImageRequest, ROW_HEIGHT, SIZE_COL_WIDTH, ScrollMode, blend_color,
    color32, fade_color, panel_path_segments, reload_panel, resort_browser_entries,
    sort_mode_label, window_rows_for,
};

/// Type-aware foreground tint blended into the row fg. Returns None for
/// directories (which use the dedicated panel_border_active color). Order
/// matters: more specific categories first.
fn entry_tint(entry: &core::DirEntry) -> Option<theme::Color> {
    if entry.is_dir || entry.name == ".." {
        return None;
    }
    if entry.is_symlink {
        // dimmed cyan — links read as references, not content
        return Some(theme::Color::rgba(0.52, 0.82, 0.92, 1.0));
    }
    let name = entry.name.as_str();
    if archive::is_container_path(std::path::Path::new(name)) {
        // amber — archives
        return Some(theme::Color::rgba(0.95, 0.75, 0.25, 1.0));
    }
    if core::is_image_name(name) {
        // magenta — images
        return Some(theme::Color::rgba(0.92, 0.55, 0.85, 1.0));
    }
    if core::is_video_name(name) {
        // bright cyan — video
        return Some(theme::Color::rgba(0.40, 0.78, 0.95, 1.0));
    }
    if core::is_audio_name(name) {
        // soft purple — audio
        return Some(theme::Color::rgba(0.72, 0.58, 0.95, 1.0));
    }
    if is_executable_name(name) {
        // green — executable
        return Some(theme::Color::rgba(0.45, 0.85, 0.45, 1.0));
    }
    if core::is_text_name(name) {
        // muted sea-green — text/documents
        return Some(theme::Color::rgba(0.32, 0.78, 0.62, 1.0));
    }
    // generic binary — neutral warm grey, very subtle
    Some(theme::Color::rgba(0.85, 0.78, 0.60, 1.0))
}

/// Compact one-line panel footer status: marked-selection size, free space
/// (for FS panels), and async-worker activity dots.
fn draw_status_line(
    ui: &mut egui::Ui,
    app: &app_state::AppState,
    panel_side: core::ActivePanel,
    colors: &theme::ThemeColors,
) {
    let panel = app.panel(panel_side);
    let browser = panel.browser();
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        let fg = color32(colors.footer_fg);
        let dim = color32(fade_color(colors.footer_fg, 0.6));

        // Marked selection: count + summed size
        if !browser.marked.is_empty() {
            let total: u64 = browser
                .entries
                .iter()
                .filter(|e| browser.marked.contains(&e.name))
                .filter_map(|e| e.size)
                .sum();
            let count = browser.marked.len();
            let label = if total > 0 {
                format!("{} marked ({})", count, core::format_size(total))
            } else {
                format!("{count} marked")
            };
            ui.colored_label(fg, label);
            ui.colored_label(dim, "·");
        }

        // FS panels: free space on the panel's drive. Cached per-frame —
        // statvfs is cheap on Linux/macOS.
        if let core::BrowserMode::Fs = browser.browser_mode
            && let Some(free) = free_space_bytes(&browser.current_path)
        {
            ui.colored_label(
                fg,
                egui::RichText::new(format!("free: {}", core::format_size(free))).monospace(),
            );
            ui.colored_label(dim, "·");
        }

        // Async worker dots: io (copy/move/delete), dir (sized), search
        let async_status = app.async_status();
        ui.colored_label(dim, "io");
        worker_dot(ui, async_status.io_in_flight > 0, colors);
        ui.colored_label(dim, "dir");
        worker_dot(ui, async_status.dir_size_pending > 0, colors);
        ui.colored_label(dim, "search");
        worker_dot(
            ui,
            matches!(async_status.search, app_state::SearchStatus::Running(_)),
            colors,
        );
    });
}

/// Filled circle if active, hollow if idle.
fn worker_dot(ui: &mut egui::Ui, active: bool, colors: &theme::ThemeColors) {
    let size = 8.0;
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
    let center = rect.center();
    let r = size * 0.32;
    if active {
        ui.painter()
            .circle_filled(center, r, color32(colors.panel_border_active));
    } else {
        ui.painter().circle_stroke(
            center,
            r,
            egui::Stroke::new(1.0_f32, color32(fade_color(colors.footer_fg, 0.5))),
        );
    }
}

#[cfg(unix)]
fn free_space_bytes(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt as _;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    // statvfs widths vary by platform; multiply via u128 to avoid overflow.
    let bavail = u128::from(stat.f_bavail);
    let frsize = u128::from(stat.f_frsize);
    Some((bavail * frsize).min(u128::from(u64::MAX)) as u64)
}

#[cfg(windows)]
fn free_space_bytes(path: &std::path::Path) -> Option<u64> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt as _;

    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            lpDirectoryName: *const u16,
            lpFreeBytesAvailableToCaller: *mut u64,
            lpTotalNumberOfBytes: *mut u64,
            lpTotalNumberOfFreeBytes: *mut u64,
        ) -> i32;
    }

    let wide: Vec<u16> = OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free_available: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok != 0 { Some(free_available) } else { None }
}

#[cfg(not(any(unix, windows)))]
fn free_space_bytes(_path: &std::path::Path) -> Option<u64> {
    None
}

fn is_executable_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.rsplit('.').next(),
        Some("sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd" | "exe" | "py" | "rb" | "pl")
    ) || matches!(
        lower.as_str(),
        "makefile" | "cmakelists.txt" | "dockerfile" | "justfile"
    )
}

/// Draw a file-type icon at `center` using the painter.
fn draw_file_icon(
    painter: &egui::Painter,
    center: egui::Pos2,
    color: egui::Color32,
    entry: &core::DirEntry,
) {
    let s = 4.5_f32; // half-size of the icon
    if entry.name == ".." {
        // Up arrow: ↑
        let tip = center + egui::Vec2::new(0.0, -s);
        let bl = center + egui::Vec2::new(-s * 0.7, s * 0.4);
        let br = center + egui::Vec2::new(s * 0.7, s * 0.4);
        painter.add(egui::Shape::convex_polygon(
            vec![tip, br, bl],
            color,
            egui::Stroke::NONE,
        ));
    } else if entry.is_dir {
        // Folder: rectangle with a tab on top-left
        let r = egui::Rect::from_center_size(
            center + egui::Vec2::new(0.0, 1.0),
            egui::Vec2::new(s * 2.0, s * 1.4),
        );
        painter.rect_filled(r, egui::CornerRadius::same(1), color);
        // Tab
        let tab = egui::Rect::from_min_size(
            egui::pos2(r.left(), r.top() - 2.5),
            egui::Vec2::new(s * 0.9, 2.5),
        );
        painter.rect_filled(tab, egui::CornerRadius::same(1), color);
    } else if archive::is_container_path(std::path::Path::new(&entry.name)) {
        // Archive: box with horizontal stripes
        let r = egui::Rect::from_center_size(center, egui::Vec2::splat(s * 2.0));
        painter.rect_stroke(
            r,
            egui::CornerRadius::same(1),
            egui::Stroke::new(1.2_f32, color),
            egui::StrokeKind::Middle,
        );
        for dy in [-2.0_f32, 0.0, 2.0] {
            let y = center.y + dy;
            painter.line_segment(
                [
                    egui::pos2(r.left() + 1.5, y),
                    egui::pos2(r.right() - 1.5, y),
                ],
                egui::Stroke::new(0.8_f32, color),
            );
        }
    } else if core::is_image_name(&entry.name) {
        // Image: square frame with a small diamond inside
        let r = egui::Rect::from_center_size(center, egui::Vec2::splat(s * 2.0));
        painter.rect_stroke(
            r,
            egui::CornerRadius::same(1),
            egui::Stroke::new(1.2_f32, color),
            egui::StrokeKind::Middle,
        );
        let d = s * 0.5;
        let diamond = vec![
            center + egui::Vec2::new(0.0, -d),
            center + egui::Vec2::new(d, 0.0),
            center + egui::Vec2::new(0.0, d),
            center + egui::Vec2::new(-d, 0.0),
        ];
        painter.add(egui::Shape::convex_polygon(
            diamond,
            color,
            egui::Stroke::NONE,
        ));
    } else if core::is_audio_name(&entry.name) || core::is_video_name(&entry.name) {
        // Media: play triangle
        let left = center.x - s * 0.6;
        let right = center.x + s * 0.8;
        let top = center.y - s;
        let bot = center.y + s;
        painter.add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(left, top),
                egui::pos2(right, center.y),
                egui::pos2(left, bot),
            ],
            color,
            egui::Stroke::NONE,
        ));
    } else if core::is_text_name(&entry.name) {
        // Text: three horizontal lines
        let w = s * 0.85;
        for dy in [-2.5_f32, 0.0, 2.5] {
            let y = center.y + dy;
            painter.line_segment(
                [egui::pos2(center.x - w, y), egui::pos2(center.x + w, y)],
                egui::Stroke::new(1.2_f32, color),
            );
        }
    } else {
        // Generic file: page with folded corner
        let r = egui::Rect::from_center_size(center, egui::Vec2::new(s * 1.6, s * 2.0));
        painter.rect_stroke(
            r,
            egui::CornerRadius::same(1),
            egui::Stroke::new(1.2_f32, color),
            egui::StrokeKind::Middle,
        );
        // Folded corner
        let fold = s * 0.5;
        let corner = vec![
            egui::pos2(r.right() - fold, r.top()),
            egui::pos2(r.right(), r.top() + fold),
            egui::pos2(r.right() - fold, r.top() + fold),
        ];
        painter.add(egui::Shape::convex_polygon(
            corner,
            color,
            egui::Stroke::NONE,
        ));
    }
}

/// An action chosen from a row's right-click context menu, dispatched after the
/// panel finishes drawing (the menu closure can't borrow `app`).
#[derive(Clone, Copy)]
enum RowAction {
    Open,
    OpenWith,
    Reveal,
    Properties,
    CopyPath,
    Rename,
    Trash,
}

pub fn draw_panel(
    ui: &mut egui::Ui,
    app: &mut app_state::AppState,
    panel_side: core::ActivePanel,
    _image_cache: &mut ImageCache,
    _image_req_tx: &mpsc::Sender<ImageRequest>,
    scroll_mode: ScrollMode,
    min_height: f32,
) -> usize {
    let available = ui.available_size();
    ui.set_min_size(available);
    // While an overlay modal is up, the panel must not react to clicks (which
    // would change selection or switch panels "behind" the dialog). Disabling
    // the Ui makes every widget below non-interactive.
    if app.any_modal_open() {
        ui.disable();
    }
    let panel_height = available.y.max(0.0).max(min_height);
    let colors = app.theme.colors();
    let is_active = app.active_panel == panel_side;
    let show_glyphs = app.settings.show_glyphs;
    let row_striping = app.settings.row_striping;

    let (
        mut entries_len,
        mut selected_index,
        header_segments,
        header_count_text,
        mut selected_label,
        mut loading,
        loading_progress,
    ) = {
        let panel = app.panel(panel_side);
        let browser = panel.browser();
        let entries_len = browser.entries.len();
        let selected_index = browser.selected_index;
        let header_segments = panel_path_segments(panel);
        let header_count_text = format!(
            "    {}/{}",
            if entries_len == 0 {
                0
            } else {
                selected_index + 1
            },
            entries_len
        );
        let selected_label = browser
            .entries
            .get(selected_index)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| "-".to_string());
        (
            entries_len,
            selected_index,
            header_segments,
            header_count_text,
            selected_label,
            browser.load.is_loading() || browser.progress_override.is_some(),
            browser.load.progress().or(browser.progress_override),
        )
    };

    let mut rows = 10usize;
    let mut clicked_index: Option<usize> = None;
    let mut open_on_double_click = false;
    let mut context_action: Option<(usize, RowAction)> = None;
    let mut new_top_index: Option<usize> = None;
    let panel_side_for_closure = panel_side;

    let mut request_raw_reload = false;
    let mut tab_clicked: Option<usize> = None;
    let mut tab_close_clicked: Option<usize> = None;
    let panel_response = ui
        .push_id("browser_panel", |ui| {
            egui::Frame::NONE
                .stroke(egui::Stroke::new(
                    1.0_f32,
                    color32(if is_active {
                        colors.panel_border_active
                    } else {
                        colors.panel_border_inactive
                    }),
                ))
                .show(ui, |ui| {
                    ui.set_min_height(panel_height);
                    ui.spacing_mut().item_spacing = egui::Vec2::new(6.0, 4.0);
                    ui.vertical(|ui| {
                        let header_height = 30.0;
                        let footer_height = 24.0;
                        let spacing = ui.spacing().item_spacing.y;

                        // Draw tab bar when multiple tabs exist
                        if app.panel(panel_side).tabs.len() > 1 {
                            let tab_count = app.panel(panel_side).tabs.len();
                            let active_tab_idx = app.panel(panel_side).active_tab;
                            ui.allocate_ui_with_layout(
                                egui::Vec2::new(ui.available_width(), 22.0),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    for i in 0..tab_count {
                                        let is_active_tab = i == active_tab_idx;
                                        let tab_bg = if is_active_tab {
                                            colors.header_bg
                                        } else {
                                            colors.footer_bg
                                        };
                                        let tab_fg = if is_active_tab {
                                            colors.header_fg
                                        } else {
                                            colors.row_fg_inactive
                                        };
                                        let label = {
                                            let browser = &app.panel(panel_side).tabs[i];
                                            browser
                                                .current_path
                                                .file_name()
                                                .and_then(|n| n.to_str())
                                                .unwrap_or("/")
                                                .to_string()
                                        };
                                        let resp = egui::Frame::NONE
                                            .fill(color32(tab_bg))
                                            .corner_radius(egui::CornerRadius {
                                                nw: 4,
                                                ne: 4,
                                                sw: 0,
                                                se: 0,
                                            })
                                            .inner_margin(egui::Margin::symmetric(6, 2))
                                            .show(ui, |ui| {
                                                ui.horizontal(|ui| {
                                                    ui.colored_label(color32(tab_fg), &label);
                                                    let x_resp = ui.colored_label(
                                                        color32(tab_fg),
                                                        egui::RichText::new("×").small(),
                                                    );
                                                    if x_resp.clicked() {
                                                        tab_close_clicked = Some(i);
                                                    }
                                                });
                                            });
                                        if resp.response.clicked() {
                                            tab_clicked = Some(i);
                                        }
                                    }
                                },
                            );
                        }

                        ui.allocate_ui_with_layout(
                            egui::Vec2::new(ui.available_width(), header_height),
                            egui::Layout::top_down(egui::Align::LEFT),
                            |ui| {
                                egui::Frame::NONE
                                    .fill(color32(colors.header_bg))
                                    .corner_radius(egui::CornerRadius::same(4))
                                    .show(ui, |ui| {
                                        let panel = app.panel_mut(panel_side);
                                        let browser = panel.browser_mut();
                                        let mut sort_mode = browser.sort_mode;
                                        let mut sort_desc = browser.sort_desc;
                                        let mut sort_changed = false;
                                        let previous_sort_mode = browser.sort_mode;

                                        let full_width = ui.available_width();
                                        let controls_width = 120.0;
                                        let gap = 24.0;
                                        let left_width =
                                            (full_width - controls_width - gap).max(0.0);
                                        let prev_spacing = ui.spacing().item_spacing;
                                        ui.spacing_mut().item_spacing.x = 0.0;
                                        ui.horizontal(|ui| {
                                            let (left_rect, _) = ui.allocate_exact_size(
                                                egui::Vec2::new(left_width, ui.available_height()),
                                                egui::Sense::hover(),
                                            );
                                            let header_font =
                                                egui::TextStyle::Body.resolve(ui.style());
                                            let mono_font =
                                                egui::TextStyle::Monospace.resolve(ui.style());
                                            let header_color = color32(colors.header_fg);
                                            let mut job = egui::text::LayoutJob::default();
                                            if loading {
                                                // Keep repainting so the spinner animates
                                                ui.ctx().request_repaint_after(
                                                    std::time::Duration::from_millis(120),
                                                );
                                                let t = ui.ctx().input(|i| i.time);
                                                let frames = [
                                                    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇",
                                                    "⠏",
                                                ];
                                                let spinner =
                                                    frames[((t * 10.0) as usize) % frames.len()];
                                                job.append(
                                                    spinner,
                                                    0.0,
                                                    egui::text::TextFormat {
                                                        font_id: mono_font.clone(),
                                                        color: header_color,
                                                        ..Default::default()
                                                    },
                                                );
                                                job.append(
                                                    " ",
                                                    0.0,
                                                    egui::text::TextFormat {
                                                        font_id: mono_font.clone(),
                                                        color: header_color,
                                                        ..Default::default()
                                                    },
                                                );
                                            }
                                            // Reserve space for the active-panel dot
                                            let dot_space =
                                                if is_active { header_font.size } else { 0.0 };
                                            let sep_color =
                                                color32(fade_color(colors.header_fg, 0.55));
                                            let text_fmt = |font: &egui::FontId, color| {
                                                egui::text::TextFormat {
                                                    font_id: font.clone(),
                                                    color,
                                                    ..Default::default()
                                                }
                                            };

                                            // Prefix (host:, drive:, archive!) — no separator
                                            // before it, gets the dot leading space.
                                            let mut leading = dot_space;
                                            if !header_segments.prefix.is_empty() {
                                                job.append(
                                                    &header_segments.prefix,
                                                    leading,
                                                    text_fmt(&header_font, header_color),
                                                );
                                                leading = 0.0;
                                            }

                                            // Breadcrumb segments with colored ASCII separators.
                                            // (Was ▸, but egui's bundled fonts don't include
                                            // U+25B8 on every system — fell back to '>' which is
                                            // guaranteed to render.)
                                            for seg in &header_segments.segments {
                                                job.append(
                                                    " > ",
                                                    leading,
                                                    text_fmt(&header_font, sep_color),
                                                );
                                                leading = 0.0;
                                                job.append(
                                                    seg,
                                                    0.0,
                                                    text_fmt(&header_font, header_color),
                                                );
                                            }

                                            // Selection / total counter
                                            job.append(
                                                &header_count_text,
                                                leading,
                                                text_fmt(&header_font, header_color),
                                            );

                                            // Progress suffix in parentheses
                                            if loading
                                                && let Some((loaded, total)) = loading_progress
                                            {
                                                let progress_str = match total {
                                                    Some(total) => format!(" ({loaded}/{total})"),
                                                    None => format!(" ({loaded})"),
                                                };
                                                job.append(
                                                    &progress_str,
                                                    0.0,
                                                    text_fmt(&header_font, header_color),
                                                );
                                            }
                                            let galley = ui.fonts_mut(|f| f.layout_job(job));
                                            let painter = ui.painter().with_clip_rect(left_rect);
                                            let pos = egui::Align2::LEFT_CENTER.anchor_size(
                                                left_rect.left_center(),
                                                galley.size(),
                                            );
                                            painter.galley(pos.min, galley, header_color);
                                            if is_active {
                                                let radius = header_font.size * 0.25;
                                                let center = egui::pos2(
                                                    pos.min.x + radius + 1.0,
                                                    left_rect.center().y,
                                                );
                                                painter.circle_filled(center, radius, header_color);
                                            }
                                            if left_width > 0.0 {
                                                ui.add_space(gap);
                                            }
                                            ui.allocate_ui_with_layout(
                                                egui::Vec2::new(
                                                    controls_width,
                                                    ui.available_height(),
                                                ),
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    egui::ComboBox::from_id_salt(
                                                        match panel_side {
                                                            core::ActivePanel::Left => {
                                                                "left_sort_mode"
                                                            }
                                                            core::ActivePanel::Right => {
                                                                "right_sort_mode"
                                                            }
                                                        },
                                                    )
                                                    .selected_text(sort_mode_label(sort_mode))
                                                    .show_ui(ui, |ui| {
                                                        sort_changed |= ui
                                                            .selectable_value(
                                                                &mut sort_mode,
                                                                core::SortMode::Name,
                                                                "Name",
                                                            )
                                                            .changed();
                                                        sort_changed |= ui
                                                            .selectable_value(
                                                                &mut sort_mode,
                                                                core::SortMode::Date,
                                                                "Date",
                                                            )
                                                            .changed();
                                                        sort_changed |= ui
                                                            .selectable_value(
                                                                &mut sort_mode,
                                                                core::SortMode::Size,
                                                                "Size",
                                                            )
                                                            .changed();
                                                        sort_changed |= ui
                                                            .selectable_value(
                                                                &mut sort_mode,
                                                                core::SortMode::Raw,
                                                                "Raw",
                                                            )
                                                            .changed();
                                                    });
                                                    let arrow = if sort_desc { "v" } else { "^" };
                                                    if ui.small_button(arrow).clicked() {
                                                        sort_desc = !sort_desc;
                                                        sort_changed = true;
                                                    }
                                                },
                                            );
                                        });
                                        ui.spacing_mut().item_spacing = prev_spacing;

                                        if sort_changed {
                                            browser.sort_mode = sort_mode;
                                            browser.sort_desc = sort_desc;
                                            if sort_mode == core::SortMode::Raw
                                                && previous_sort_mode != core::SortMode::Raw
                                            {
                                                request_raw_reload = true;
                                            } else {
                                                resort_browser_entries(browser);
                                            }
                                        }
                                    });
                            },
                        );

                        if request_raw_reload {
                            reload_panel(app, panel_side);
                            let panel = app.panel(panel_side);
                            let browser = panel.browser();
                            entries_len = browser.entries.len();
                            selected_index =
                                browser.selected_index.min(entries_len.saturating_sub(1));
                            selected_label = browser
                                .entries
                                .get(selected_index)
                                .map(|e| e.name.clone())
                                .unwrap_or_else(|| "-".to_string());
                            loading = browser.load.is_loading();
                        }

                        let list_height =
                            (ui.available_height() - footer_height - spacing).max(0.0);
                        rows = window_rows_for(list_height, ui.spacing().item_spacing.y);
                        let mut visible_range = 0..0;

                        let mut scroll_target = None;
                        if is_active && entries_len > 0 {
                            let row_height = ROW_HEIGHT + ui.spacing().item_spacing.y;
                            let total_height = (row_height * entries_len as f32
                                - ui.spacing().item_spacing.y)
                                .max(0.0);
                            scroll_target = list_scroll_target(
                                scroll_mode,
                                selected_index,
                                row_height,
                                total_height,
                                list_height,
                            );
                        }

                        ui.allocate_ui_with_layout(
                            egui::Vec2::new(ui.available_width(), list_height),
                            egui::Layout::top_down(egui::Align::LEFT),
                            |ui| {
                                let mut scroll = egui::ScrollArea::vertical()
                                    .id_salt(match panel_side_for_closure {
                                        core::ActivePanel::Left => "left_list",
                                        core::ActivePanel::Right => "right_list",
                                    })
                                    .auto_shrink([false, false]);
                                if let Some(offset) = scroll_target {
                                    scroll = scroll.vertical_scroll_offset(offset);
                                }
                                scroll.show_rows(ui, ROW_HEIGHT, entries_len, |ui, row_range| {
                                    visible_range = row_range.clone();
                                    for idx in row_range {
                                        let (entry, rename_active, is_marked) = {
                                            let browser =
                                                app.panel(panel_side_for_closure).browser();
                                            let entry = browser.entries[idx].clone();
                                            let rename_active = browser
                                                .inline_rename
                                                .as_ref()
                                                .is_some_and(|rename| rename.index == idx);
                                            let is_marked = browser.marked.contains(&entry.name);
                                            (entry, rename_active, is_marked)
                                        };
                                        let is_selected = selected_index == idx;
                                        let stripe = row_striping && idx % 2 == 0;
                                        let bg = if is_selected {
                                            if is_active {
                                                colors.row_bg_selected_active
                                            } else {
                                                colors.row_bg_selected_inactive
                                            }
                                        } else if stripe {
                                            colors.row_bg_stripe
                                        } else {
                                            theme::Color::rgba(0.0, 0.0, 0.0, 0.0)
                                        };
                                        let fg = if is_selected && is_active {
                                            colors.row_fg_selected
                                        } else if is_active {
                                            colors.row_fg_active
                                        } else {
                                            colors.row_fg_inactive
                                        };
                                        let mut fg = fg;
                                        let is_hidden = entry.name.starts_with('.')
                                            && entry.name.as_str() != "..";
                                        let file_tint = entry_tint(&entry);
                                        if !is_selected && let Some(tint) = file_tint {
                                            let factor = if is_active { 0.42 } else { 0.32 };
                                            fg = blend_color(fg, tint, factor);
                                        }
                                        if is_hidden && !is_selected {
                                            fg = fade_color(fg, 0.55);
                                        }
                                        // Marked entries get a colored gutter dot rather than
                                        // a background invert — keeps marked+selected readable.

                                        let (rect, response) = ui.allocate_exact_size(
                                            egui::Vec2::new(ui.available_width(), ROW_HEIGHT),
                                            egui::Sense::click(),
                                        );
                                        // Right-click context menu (skipped for ".."). Items that
                                        // need a local path are hidden for remote / in-archive rows.
                                        if entry.name != ".." {
                                            // Select the row under the cursor as the menu opens so
                                            // it's clear what the actions apply to.
                                            if response.secondary_clicked() {
                                                clicked_index = Some(idx);
                                            }
                                            let is_fs = matches!(
                                                entry.location,
                                                core::EntryLocation::Fs(_)
                                            );
                                            let can_rename = !matches!(
                                                entry.location,
                                                core::EntryLocation::Container { .. }
                                            );
                                            response.context_menu(|ui| {
                                                if ui.button("Open").clicked() {
                                                    context_action = Some((idx, RowAction::Open));
                                                    ui.close();
                                                }
                                                if ui.button("Open with default app").clicked() {
                                                    context_action = Some((idx, RowAction::OpenWith));
                                                    ui.close();
                                                }
                                                if is_fs {
                                                    if ui.button("Reveal in file manager").clicked() {
                                                        context_action =
                                                            Some((idx, RowAction::Reveal));
                                                        ui.close();
                                                    }
                                                    if ui.button("Properties").clicked() {
                                                        context_action =
                                                            Some((idx, RowAction::Properties));
                                                        ui.close();
                                                    }
                                                }
                                                if ui.button("Copy path").clicked() {
                                                    context_action =
                                                        Some((idx, RowAction::CopyPath));
                                                    ui.close();
                                                }
                                                if can_rename && ui.button("Rename").clicked() {
                                                    context_action = Some((idx, RowAction::Rename));
                                                    ui.close();
                                                }
                                                if is_fs {
                                                    ui.separator();
                                                    if ui.button("Move to Trash").clicked() {
                                                        context_action =
                                                            Some((idx, RowAction::Trash));
                                                        ui.close();
                                                    }
                                                }
                                            });
                                        }
                                        ui.painter().rect_filled(
                                            rect,
                                            egui::CornerRadius::same(3),
                                            color32(bg),
                                        );

                                        // Gutter dot for marked entries
                                        if is_marked {
                                            let dot_center =
                                                egui::pos2(rect.left() + 3.5, rect.center().y);
                                            ui.painter().circle_filled(
                                                dot_center,
                                                2.5,
                                                color32(colors.row_fg_selected),
                                            );
                                        }

                                        let font_id = egui::TextStyle::Body.resolve(ui.style());
                                        let icon_color = if entry.is_dir {
                                            colors.panel_border_active
                                        } else if is_selected {
                                            fg
                                        } else if let Some(tint) = file_tint {
                                            blend_color(fg, tint, 0.85)
                                        } else {
                                            fg
                                        };
                                        let icon_color = if is_hidden && !is_selected {
                                            fade_color(icon_color, 0.55)
                                        } else {
                                            icon_color
                                        };
                                        let ic = color32(icon_color);
                                        let center =
                                            egui::pos2(rect.left() + 12.0, rect.center().y);
                                        // Vector-drawn icons have no font dependency
                                        // and render identically across systems —
                                        // egui's bundled fonts don't include enough
                                        // of the emoji set to use them reliably.
                                        if show_glyphs {
                                            draw_file_icon(ui.painter(), center, ic, &entry);
                                        }

                                        let mut size_text =
                                            entry.size.map(core::format_size).unwrap_or_default();
                                        if size_text.is_empty()
                                            && entry.is_dir
                                            && let core::EntryLocation::Fs(path) = &entry.location
                                            && app.dir_size_pending.contains(path)
                                        {
                                            size_text = "…".to_string();
                                        }
                                        if !size_text.is_empty() {
                                            ui.painter().text(
                                                egui::pos2(rect.right() - 8.0, rect.center().y),
                                                egui::Align2::RIGHT_CENTER,
                                                size_text,
                                                font_id.clone(),
                                                color32(fg),
                                            );
                                        }
                                        // Date column between name and size
                                        if let Some(mtime) = entry.modified {
                                            let date_text = core::format_date(mtime);
                                            ui.painter().text(
                                                egui::pos2(
                                                    rect.right() - SIZE_COL_WIDTH - 4.0,
                                                    rect.center().y,
                                                ),
                                                egui::Align2::RIGHT_CENTER,
                                                date_text,
                                                font_id.clone(),
                                                color32(fade_color(fg, 0.7)),
                                            );
                                        }
                                        let right_cols = SIZE_COL_WIDTH + DATE_COL_WIDTH;
                                        let name_min =
                                            rect.left_center() + egui::Vec2::new(22.0, 0.0);
                                        let name_rect = egui::Rect::from_min_max(
                                            egui::pos2(name_min.x, rect.top()),
                                            egui::pos2(rect.right() - right_cols, rect.bottom()),
                                        );
                                        if rename_active {
                                            ui.scope_builder(
                                                egui::UiBuilder::new().max_rect(name_rect),
                                                |ui| {
                                                    ui.set_clip_rect(name_rect);
                                                    // Force a high-contrast selection
                                                    // background. Without this override the
                                                    // default egui selection.bg_fill is the
                                                    // same family of blue as the selected-row
                                                    // bg, so the user can't see what they've
                                                    // highlighted while typing/editing.
                                                    let sel_color = if is_selected {
                                                        // Yellow on blue: maximum contrast.
                                                        egui::Color32::from_rgba_unmultiplied(
                                                            240, 200, 70, 180,
                                                        )
                                                    } else {
                                                        egui::Color32::from_rgba_unmultiplied(
                                                            80, 140, 220, 180,
                                                        )
                                                    };
                                                    ui.visuals_mut().selection.bg_fill = sel_color;
                                                    let rename = app
                                                        .panel_mut(panel_side_for_closure)
                                                        .browser_mut()
                                                        .inline_rename
                                                        .as_mut()
                                                        .expect("rename active");
                                                    let response = ui.add_sized(
                                                        name_rect.size(),
                                                        egui::TextEdit::singleline(
                                                            &mut rename.text,
                                                        )
                                                        .font(egui::TextStyle::Body)
                                                        .id_source(match panel_side_for_closure {
                                                            core::ActivePanel::Left => {
                                                                "inline_rename_left"
                                                            }
                                                            core::ActivePanel::Right => {
                                                                "inline_rename_right"
                                                            }
                                                        }),
                                                    );
                                                    if rename.focus {
                                                        response.request_focus();
                                                        // For Rename, pre-select the stem (name
                                                        // before the last dot) so typing replaces
                                                        // it while keeping the extension. For
                                                        // NewFile/NewDir, select the whole thing.
                                                        let text_len = rename.text.len();
                                                        let sel_end = match rename.kind {
                                                            app_state::InlineEditKind::Rename => {
                                                                rename
                                                                    .text
                                                                    .rfind('.')
                                                                    .filter(|&i| i > 0)
                                                                    .unwrap_or(text_len)
                                                            }
                                                            _ => text_len,
                                                        };
                                                        // TextEdit state doesn't exist until the
                                                        // widget has been drawn once — keep the
                                                        // focus flag set until we successfully
                                                        // apply the selection.
                                                        if let Some(mut state) =
                                                            egui::TextEdit::load_state(
                                                                ui.ctx(),
                                                                response.id,
                                                            )
                                                        {
                                                            state.cursor.set_char_range(Some(
                                                                egui::text::CCursorRange::two(
                                                                    egui::text::CCursor::new(0),
                                                                    egui::text::CCursor::new(
                                                                        sel_end,
                                                                    ),
                                                                ),
                                                            ));
                                                            state.store(ui.ctx(), response.id);
                                                            rename.focus = false;
                                                        }
                                                    }
                                                },
                                            );
                                        } else if entry.is_symlink {
                                            let arrow_text = format!("{} -> ", entry.name);
                                            let target_text =
                                                entry.link_target.as_deref().unwrap_or("?");
                                            let is_broken = entry.size.is_none() && !entry.is_dir;
                                            let target_color = if is_broken {
                                                color32(theme::Color::rgba(0.9, 0.3, 0.3, 1.0))
                                            } else {
                                                color32(fade_color(fg, 0.65))
                                            };
                                            let full = format!("{arrow_text}{target_text}");
                                            let galley = ui.painter().layout_no_wrap(
                                                full,
                                                font_id.clone(),
                                                color32(fg),
                                            );
                                            let arrow_galley = ui.painter().layout_no_wrap(
                                                arrow_text.clone(),
                                                font_id.clone(),
                                                color32(fg),
                                            );
                                            let arrow_w = arrow_galley.size().x;
                                            let clipped = ui.painter().with_clip_rect(name_rect);
                                            clipped.text(
                                                name_min,
                                                egui::Align2::LEFT_CENTER,
                                                arrow_text,
                                                font_id.clone(),
                                                color32(fg),
                                            );
                                            let target_pos =
                                                name_min + egui::Vec2::new(arrow_w, 0.0);
                                            clipped.text(
                                                target_pos,
                                                egui::Align2::LEFT_CENTER,
                                                target_text,
                                                font_id.clone(),
                                                target_color,
                                            );
                                            if galley.size().x > name_rect.width() {
                                                clipped.text(
                                                    egui::pos2(
                                                        name_rect.right() - 12.0,
                                                        name_rect.center().y,
                                                    ),
                                                    egui::Align2::LEFT_CENTER,
                                                    "\u{2026}",
                                                    font_id,
                                                    color32(fg),
                                                );
                                            }
                                        } else {
                                            let galley = ui.painter().layout_no_wrap(
                                                entry.name.clone(),
                                                font_id.clone(),
                                                color32(fg),
                                            );
                                            let clipped = ui.painter().with_clip_rect(name_rect);
                                            clipped.text(
                                                name_min,
                                                egui::Align2::LEFT_CENTER,
                                                &entry.name,
                                                font_id.clone(),
                                                color32(fg),
                                            );
                                            if galley.size().x > name_rect.width() {
                                                clipped.text(
                                                    egui::pos2(
                                                        name_rect.right() - 12.0,
                                                        name_rect.center().y,
                                                    ),
                                                    egui::Align2::LEFT_CENTER,
                                                    "\u{2026}",
                                                    font_id,
                                                    color32(fg),
                                                );
                                            }
                                        }

                                        if response.clicked_by(egui::PointerButton::Primary) {
                                            clicked_index = Some(idx);
                                        }
                                        if response.double_clicked_by(egui::PointerButton::Primary)
                                        {
                                            clicked_index = Some(idx);
                                            open_on_double_click = true;
                                        }
                                    }
                                });
                            },
                        );

                        if entries_len > 0 {
                            new_top_index = Some(visible_range.start.min(entries_len - 1));
                        } else {
                            new_top_index = Some(0);
                        }

                        ui.allocate_ui_with_layout(
                            egui::Vec2::new(ui.available_width(), footer_height),
                            egui::Layout::top_down(egui::Align::LEFT),
                            |ui| {
                                egui::Frame::NONE
                                    .fill(color32(colors.footer_bg))
                                    .corner_radius(egui::CornerRadius::same(4))
                                    .show(ui, |ui| {
                                        let search_open = is_active
                                            && app.search_ui == app_state::SearchUiState::Open;
                                        if !search_open {
                                            draw_status_line(
                                                ui,
                                                app,
                                                panel_side_for_closure,
                                                &colors,
                                            );
                                        }
                                        if search_open {
                                            ui.horizontal(|ui| {
                                                ui.colored_label(
                                                    color32(colors.footer_fg),
                                                    "Search:",
                                                );
                                                let id = ui.make_persistent_id("search_edit");
                                                let response = egui::TextEdit::singleline(
                                                    &mut app.search_query,
                                                )
                                                .id(id)
                                                .show(ui);
                                                if app.search_focus {
                                                    response.response.request_focus();
                                                    // Select all so typing replaces the old query.
                                                    if let Some(mut state) =
                                                        egui::TextEdit::load_state(ui.ctx(), id)
                                                    {
                                                        state.cursor.set_char_range(Some(
                                                            egui::text::CCursorRange::two(
                                                                egui::text::CCursor::new(0),
                                                                egui::text::CCursor::new(
                                                                    app.search_query.len(),
                                                                ),
                                                            ),
                                                        ));
                                                        state.store(ui.ctx(), id);
                                                    }
                                                    app.search_focus = false;
                                                }
                                            });
                                        }
                                    });
                            },
                        );
                    });
                })
        })
        .inner;

    if is_active {
        // 2px colored stripe down the left edge — reads as the active panel in
        // peripheral vision faster than the surrounding border.
        let rect = panel_response.response.rect;
        let stripe = egui::Rect::from_min_max(rect.min, egui::pos2(rect.min.x + 2.0, rect.max.y));
        ui.painter().rect_filled(
            stripe,
            egui::CornerRadius::ZERO,
            color32(colors.panel_border_active),
        );
    }

    if panel_response.response.contains_pointer() && ui.input(|i| i.pointer.any_pressed()) {
        app.active_panel = panel_side;
    }

    if let Some(top) = new_top_index {
        app.panel_mut(panel_side).browser_mut().top_index = top;
    }

    if let Some(idx) = clicked_index {
        app.active_panel = panel_side;
        app.select_entry(idx, rows);
        if open_on_double_click {
            open_selected(app);
        }
    }

    if let Some((idx, action)) = context_action {
        app.active_panel = panel_side;
        app.select_entry(idx, rows);
        match action {
            RowAction::Open => open_selected(app),
            RowAction::OpenWith => crate::input::open_selected_external(app),
            RowAction::Reveal => crate::input::reveal_selected_in_file_manager(app),
            RowAction::Properties => crate::input::show_properties_of_selected(app),
            RowAction::CopyPath => crate::input::copy_selected_paths(app, ui.ctx()),
            RowAction::Rename => app.prepare_rename_selected(),
            RowAction::Trash => crate::input::trash_selected(app),
        }
    }

    // Handle tab close before tab switch (close may affect indices)
    if let Some(i) = tab_close_clicked {
        app.active_panel = panel_side;
        let panel = app.panel_mut(panel_side);
        if panel.tabs.len() > 1 {
            panel.tabs.remove(i);
            if panel.active_tab >= panel.tabs.len() {
                panel.active_tab = panel.tabs.len() - 1;
            } else if panel.active_tab > i {
                panel.active_tab -= 1;
            }
        }
    } else if let Some(i) = tab_clicked {
        app.active_panel = panel_side;
        app.panel_mut(panel_side).active_tab = i;
    }

    rows
}

/// Where the list should be scrolled to, or `None` to leave it where it is.
///
/// Only a selection or directory change moves the view. Scrolling with the
/// wheel leaves the selection behind, and pulling it back into sight then
/// would fight the wheel: every notch up was undone on the next frame, so the
/// list bounced and never reached the top.
fn list_scroll_target(
    mode: ScrollMode,
    selected_index: usize,
    row_height: f32,
    total_height: f32,
    list_height: f32,
) -> Option<f32> {
    if mode != ScrollMode::ForceActive {
        return None;
    }
    if total_height <= list_height {
        return Some(0.0);
    }
    let center_offset = (list_height - row_height) * 0.5;
    let max_offset = (total_height - list_height).max(0.0);
    Some((selected_index as f32 * row_height - center_offset).clamp(0.0, max_offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROW: f32 = 20.0;
    const LIST: f32 = 200.0;
    // 100 rows, so there is plenty to scroll through.
    const TOTAL: f32 = ROW * 100.0;

    #[test]
    fn a_wheel_scroll_is_left_alone() {
        // The selection has not moved, so neither should the view, however far
        // from the selection the user has scrolled.
        assert_eq!(
            list_scroll_target(ScrollMode::Default, 50, ROW, TOTAL, LIST),
            None
        );
        assert_eq!(
            list_scroll_target(ScrollMode::Default, 0, ROW, TOTAL, LIST),
            None
        );
    }

    #[test]
    fn a_moved_selection_is_brought_into_view() {
        let target = list_scroll_target(ScrollMode::ForceActive, 50, ROW, TOTAL, LIST)
            .expect("selection moved, so the view should follow");
        // Centred on the selection.
        assert!((target - (50.0 * ROW - (LIST - ROW) * 0.5)).abs() < 0.5);
    }

    #[test]
    fn the_view_stays_within_the_list() {
        let top = list_scroll_target(ScrollMode::ForceActive, 0, ROW, TOTAL, LIST).unwrap();
        assert_eq!(top, 0.0, "the first row cannot scroll above the top");
        let bottom = list_scroll_target(ScrollMode::ForceActive, 99, ROW, TOTAL, LIST).unwrap();
        assert_eq!(
            bottom,
            TOTAL - LIST,
            "the last row cannot scroll past the end"
        );
    }

    #[test]
    fn a_list_that_fits_never_scrolls() {
        assert_eq!(
            list_scroll_target(ScrollMode::ForceActive, 3, ROW, ROW * 4.0, LIST),
            Some(0.0)
        );
    }
}
