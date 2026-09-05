use fileman::app_state::{AsyncStatus, ErrorLogEntry, SearchStatus, UpdateStatus};
use fileman::theme;

use crate::color32;

pub fn draw_help(
    ui: &mut egui::Ui,
    theme: &theme::Theme,
    is_focused: bool,
    min_height: f32,
    async_status: &AsyncStatus,
    error_log: &[ErrorLogEntry],
) -> bool {
    let mut install_requested = false;
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

                        // Update status
                        ui.add_space(6.0);
                        install_requested = draw_update_status(ui, &colors, &async_status.update);

                        // Async workers status
                        ui.add_space(10.0);
                        ui.colored_label(color32(colors.preview_text), "Async Workers");
                        ui.add_space(6.0);
                        draw_async_status(ui, &colors, async_status);

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
    install_requested
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

fn draw_update_status(
    ui: &mut egui::Ui,
    colors: &theme::ThemeColors,
    status: &UpdateStatus,
) -> bool {
    let mut install_requested = false;
    match status {
        UpdateStatus::Disabled => {}
        UpdateStatus::Checking => {
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.spinner();
                ui.colored_label(color32(colors.row_fg_inactive), "Checking for updates...");
            });
        }
        UpdateStatus::UpToDate => {
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.colored_label(color32(colors.row_fg_inactive), "Up to date.");
            });
        }
        UpdateStatus::Available(version) => {
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.colored_label(
                    color32(colors.row_fg_selected),
                    egui::RichText::new(format!("Update available: v{version}")).strong(),
                );
                if ui.button("Install").clicked() {
                    install_requested = true;
                }
            });
        }
        UpdateStatus::Installing(version) => {
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.spinner();
                ui.colored_label(
                    color32(colors.row_fg_inactive),
                    format!("Installing v{version}..."),
                );
            });
        }
        UpdateStatus::Installed(version) => {
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.colored_label(
                    color32(colors.row_fg_selected),
                    egui::RichText::new(format!("v{version} installed — restart to use")).strong(),
                );
            });
        }
        UpdateStatus::Failed(err) => {
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.colored_label(
                    color32(colors.row_fg_inactive),
                    format!("Update failed: {err}"),
                );
            });
        }
    }
    install_requested
}

fn draw_async_status(ui: &mut egui::Ui, colors: &theme::ThemeColors, status: &AsyncStatus) {
    // IO worker
    let io_label = if status.io_in_flight == 0 {
        "idle".to_string()
    } else if status.io_cancel_requested {
        format!("{} tasks (cancelling)", status.io_in_flight)
    } else {
        format!("{} tasks in flight", status.io_in_flight)
    };
    draw_worker_row(ui, colors, "IO", &io_label, status.io_in_flight > 0);

    // Dir size worker
    let dir_label = if status.dir_size_pending == 0 {
        "idle".to_string()
    } else {
        format!("{} pending", status.dir_size_pending)
    };
    draw_worker_row(
        ui,
        colors,
        "Dir size",
        &dir_label,
        status.dir_size_pending > 0,
    );

    // Search worker
    let (search_label, search_active) = match status.search {
        SearchStatus::Idle => ("idle".to_string(), false),
        SearchStatus::Running(progress) => (
            format!(
                "scanning ({} scanned, {} matched)",
                progress.scanned, progress.matched
            ),
            true,
        ),
        SearchStatus::Done(progress) => (
            format!(
                "done ({} scanned, {} matched)",
                progress.scanned, progress.matched
            ),
            false,
        ),
    };
    draw_worker_row(ui, colors, "Search", &search_label, search_active);
}

fn draw_worker_row(
    ui: &mut egui::Ui,
    colors: &theme::ThemeColors,
    name: &str,
    status: &str,
    active: bool,
) {
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        ui.colored_label(
            color32(colors.row_fg_selected),
            egui::RichText::new(format!("{name}:")).monospace().strong(),
        );
        let color = if active {
            colors.row_fg_active
        } else {
            colors.row_fg_inactive
        };
        ui.colored_label(color32(color), status);
    });
}
