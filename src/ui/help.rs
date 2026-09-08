use fileman::app_state::{AsyncStatus, ErrorLogEntry};
use fileman::theme;

use crate::color32;

pub fn draw_help(
    ui: &mut egui::Ui,
    theme: &theme::Theme,
    is_focused: bool,
    min_height: f32,
    async_status: &AsyncStatus,
    error_log: &[ErrorLogEntry],
) {
    let colors = theme.colors();
    ui.push_id("help_panel", |ui| {
        let version = env!("CARGO_PKG_VERSION");
        let shortcuts = [
            ("Enter", "Open"),
            ("Shift+Enter", "Open with system default app"),
            ("Ctrl+Shift+O", "Reveal in file manager"),
            ("Right-click", "Context menu"),
            ("Tab", "Switch panels"),
            ("Ctrl+T", "New tab"),
            ("Ctrl+W", "Close tab"),
            ("Ctrl+Tab / Ctrl+Shift+Tab", "Next / prev tab"),
            ("Ctrl+U", "Swap panels"),
            ("Alt+Left / Alt+Right", "Back / forward"),
            ("Backspace / Ctrl+PgUp", "Parent folder"),
            ("Ctrl+PgDn", "Open selected"),
            ("Ctrl+Left / Ctrl+Right", "Open selected dir in other panel"),
            ("F3 / Ctrl+P", "Preview"),
            ("F4 / Ctrl+E", "Edit"),
            ("Shift+F4 / Ctrl+N", "New file"),
            ("F7 / Ctrl+D", "New directory"),
            ("Insert / Ctrl+I", "Mark / unmark"),
            ("Shift+Up / Shift+Down", "Extend selection"),
            ("Shift+F6 / Ctrl+Shift+M", "Rename"),
            ("F5 / Ctrl+C", "Copy"),
            ("Ctrl+Shift+C", "Copy path to clipboard"),
            ("Ctrl+,", "Settings"),
            ("Alt+F5 / Ctrl+A", "Pack (create archive)"),
            ("F6 / Ctrl+M", "Move"),
            ("F8 / Ctrl+X", "Delete"),
            ("Space", "Compute folder size"),
            ("Alt+F7", "Search by name"),
            ("Shift+Alt+F7", "Search by content"),
            ("Ctrl+F", "Find in preview"),
            ("Alt+Enter", "Properties"),
            ("F2 / Ctrl+R", "Refresh"),
            ("Ctrl+G", "Quick jump"),
            ("F9", "Toggle theme"),
            ("F10", "Theme picker"),
            ("F1 / Ctrl+H", "Help"),
        ];
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
                ui.set_min_size(egui::Vec2::new(ui.available_width(), min_height));
                egui::Frame::NONE
                    .fill(color32(colors.preview_header_bg))
                    .show(ui, |ui| {
                        ui.colored_label(color32(colors.preview_header_fg), "Help (Tab to return)");
                    });
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(8.0);
                        ui.colored_label(
                            color32(colors.preview_text),
                            format!("FileMan {version}"),
                        );
                        ui.colored_label(
                            color32(colors.row_fg_inactive),
                            "Author: Dzmitry Malyshau",
                        );

                        ui.colored_label(
                            color32(colors.row_fg_inactive),
                            format!("GPU: {}", async_status.gpu_info),
                        );

                        ui.add_space(6.0);
                        ui.hyperlink_to(
                            "Releases",
                            "https://github.com/navigato-rs/fileman/releases/latest",
                        );

                        navigato_support::show(ui, fileman::SUPPORT);

                        // Recent errors
                        if !error_log.is_empty() {
                            ui.add_space(10.0);
                            ui.colored_label(color32(colors.preview_text), "Recent Errors");
                            ui.add_space(6.0);
                            let now = std::time::Instant::now();
                            let recent: Vec<&ErrorLogEntry> =
                                error_log.iter().rev().take(10).collect();
                            for entry in recent {
                                ui.horizontal(|ui| {
                                    ui.add_space(10.0);
                                    ui.colored_label(
                                        color32(colors.row_fg_inactive),
                                        egui::RichText::new(relative_time(now, entry.when))
                                            .monospace(),
                                    );
                                    ui.colored_label(
                                        color32(colors.row_fg_selected),
                                        egui::RichText::new(format!("[{}]", entry.source))
                                            .monospace(),
                                    );
                                    ui.colored_label(
                                        color32(colors.row_fg_inactive),
                                        entry.message.lines().next().unwrap_or(""),
                                    );
                                });
                            }
                        }

                        ui.add_space(10.0);
                        ui.colored_label(color32(colors.preview_text), "Shortcuts");
                        ui.add_space(6.0);
                        for (keys, desc) in shortcuts {
                            ui.horizontal(|ui| {
                                ui.add_space(10.0);
                                ui.colored_label(
                                    color32(colors.row_fg_selected),
                                    egui::RichText::new(keys).monospace().strong(),
                                );
                                ui.colored_label(color32(colors.row_fg_inactive), desc);
                            });
                        }
                    });
            });
    });
}

/// Compact relative-time format for the error log: "3s", "12m", "2h", "5d".
fn relative_time(now: std::time::Instant, when: std::time::Instant) -> String {
    let elapsed = now.saturating_duration_since(when).as_secs();
    if elapsed < 60 {
        format!("{elapsed:>3}s")
    } else if elapsed < 3600 {
        format!("{:>3}m", elapsed / 60)
    } else if elapsed < 86400 {
        format!("{:>3}h", elapsed / 3600)
    } else {
        format!("{:>3}d", elapsed / 86400)
    }
}
