use egui;

use fileman::app_state;

use crate::{color32, input};

pub fn draw_confirmation(ctx: &egui::Context, app: &mut app_state::AppState) {
    let op = match app.pending_op().cloned() {
        Some(op) => op,
        None => return,
    };
    let enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));
    let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    let (title, body) = pending_op_text(&op);
    let colors = app.theme.colors();
    let screen = ctx.content_rect();
    let overlay_layer = egui::LayerId::new(egui::Order::Middle, "confirm_overlay".into());
    ctx.layer_painter(overlay_layer).rect_filled(
        screen,
        egui::CornerRadius::ZERO,
        egui::Color32::from_black_alpha(160),
    );
    let mut confirmed = false;
    let mut cancelled = false;
    let is_rename = matches!(
        op,
        app_state::PendingOp::Rename { .. } | app_state::PendingOp::Pack { .. }
    );
    let collisions = &app.pending_collisions;
    let overwrite = !collisions.is_empty();

    egui::Window::new(title)
        .order(egui::Order::Foreground)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.add_space(4.0);
            ui.colored_label(color32(colors.row_fg_active), body);
            if overwrite {
                ui.add_space(8.0);
                let warn = egui::Color32::from_rgb(230, 170, 70);
                let header = if collisions.len() == 1 {
                    "1 item already exists and will be overwritten:".to_string()
                } else {
                    format!(
                        "{} items already exist and will be overwritten:",
                        collisions.len()
                    )
                };
                ui.colored_label(warn, header);
                for name in collisions.iter().take(6) {
                    ui.colored_label(warn, format!("  • {name}"));
                }
                if collisions.len() > 6 {
                    ui.colored_label(warn, format!("  … and {} more", collisions.len() - 6));
                }
            }
            if is_rename {
                ui.add_space(8.0);
                let mut name = app.rename_input.clone().unwrap_or_default();
                let response = ui.add(
                    egui::TextEdit::singleline(&mut name)
                        .desired_width(260.0)
                        .hint_text("New name"),
                );
                if app.rename_focus {
                    response.request_focus();
                    app.rename_focus = false;
                }
                app.rename_input = Some(name);
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    confirmed = true;
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let ok = ui.add(egui::Button::new("OK").min_size(egui::vec2(80.0, 0.0)));
                    let cancel =
                        ui.add(egui::Button::new("Cancel").min_size(egui::vec2(80.0, 0.0)));
                    if ok.clicked() {
                        confirmed = true;
                    }
                    if cancel.clicked() {
                        cancelled = true;
                    }
                });
            } else {
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let yes_label = if overwrite { "Overwrite" } else { "Yes" };
                    let yes = ui.add(egui::Button::new(yes_label).min_size(egui::vec2(80.0, 0.0)));
                    let no = ui.add(egui::Button::new("No").min_size(egui::vec2(80.0, 0.0)));
                    if yes.clicked() {
                        confirmed = true;
                    }
                    if no.clicked() {
                        cancelled = true;
                    }
                });
            }
        });

    if !is_rename {
        if enter {
            confirmed = true;
        }
        if escape {
            cancelled = true;
        }
    }

    if confirmed {
        input::confirm_pending_op(app);
    } else if cancelled {
        app.clear_pending_op();
    }
}

pub fn draw_progress_modal(ctx: &egui::Context, app: &app_state::AppState) {
    if app.io_in_flight == 0 {
        return;
    }

    let colors = app.theme.colors();
    let screen = ctx.content_rect();
    let overlay_layer = egui::LayerId::new(egui::Order::Middle, "progress_overlay".into());
    ctx.layer_painter(overlay_layer).rect_filled(
        screen,
        egui::CornerRadius::ZERO,
        egui::Color32::from_black_alpha(120),
    );

    egui::Window::new(app.io_verb)
        .order(egui::Order::Foreground)
        .collapsible(false)
        .resizable(false)
        .default_width(420.0)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.add_space(6.0);
            // Title already carries the verb ("Copying"…); the body shows only
            // the progress detail so it doesn't read "Copying / Copying…".
            let header = if app.io_cancel_requested {
                Some("Cancelling…".to_string())
            } else if app.io_batch_total > 1 {
                let completed = app.io_batch_total.saturating_sub(app.io_in_flight);
                let current = completed + 1;
                Some(format!(
                    "File {} of {}",
                    current.min(app.io_batch_total),
                    app.io_batch_total
                ))
            } else {
                None
            };
            if let Some(header) = header {
                ui.colored_label(color32(colors.row_fg_active), header);
            }

            // Current file name. Mid-truncate so the leading dir context
            // and the filename suffix both stay visible.
            if let Some(name) = app.transfer_progress.current_name() {
                let display = if name.chars().count() > 64 {
                    let head: String = name.chars().take(28).collect();
                    let tail: String = name
                        .chars()
                        .rev()
                        .take(28)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    format!("{head}…{tail}")
                } else {
                    name
                };
                ui.colored_label(
                    color32(colors.row_fg_inactive),
                    egui::RichText::new(display).monospace(),
                );
            }

            ui.add_space(8.0);
            let (done, total) = app.transfer_progress.snapshot();
            let items = app
                .transfer_progress
                .items_done
                .load(std::sync::atomic::Ordering::Relaxed);
            if total > 0 {
                let fraction = (done as f32 / total as f32).clamp(0.0, 1.0);
                let done_fmt = fileman::core::format_size(done);
                let total_fmt = fileman::core::format_size(total);
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .text(format!("{done_fmt} / {total_fmt}"))
                        .animate(false),
                );
            } else if done > 0 {
                let done_fmt = fileman::core::format_size(done);
                ui.add(
                    egui::ProgressBar::new(0.0)
                        .text(format!("{done_fmt}…"))
                        .animate(true),
                );
            } else if items > 0 {
                ui.colored_label(
                    color32(colors.row_fg_active),
                    format!("Deleted {items} items…"),
                );
            } else {
                ui.add(egui::ProgressBar::new(0.0).animate(true));
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
            ui.add_space(6.0);
            ui.colored_label(color32(colors.row_fg_inactive), "Esc: cancel");
        });
}

pub fn draw_discard_modal(ctx: &egui::Context, app: &mut app_state::AppState) {
    let colors = app.theme.colors();
    let Some(side) = app.edit_panel_side() else {
        return;
    };
    let screen = ctx.content_rect();
    let overlay_layer = egui::LayerId::new(egui::Order::Middle, "discard_overlay".into());
    ctx.layer_painter(overlay_layer).rect_filled(
        screen,
        egui::CornerRadius::ZERO,
        egui::Color32::from_black_alpha(120),
    );

    let mut action: Option<bool> = None;
    egui::Window::new("Discard changes?")
        .order(egui::Order::Foreground)
        .collapsible(false)
        .resizable(false)
        .fixed_size(egui::Vec2::new(360.0, 140.0))
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(4.0);
                ui.colored_label(color32(colors.row_fg_active), "Discard unsaved edits?");
                ui.add_space(14.0);
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    let yes = egui::Button::new("Discard").min_size(egui::Vec2::new(120.0, 0.0));
                    let no =
                        egui::Button::new("Keep Editing").min_size(egui::Vec2::new(140.0, 0.0));
                    if ui.add(yes).clicked() {
                        action = Some(true);
                    }
                    if ui.add(no).clicked() {
                        action = Some(false);
                    }
                });
                ui.add_space(6.0);
            });
        });
    if let Some(accept) = action {
        let panel = app.panel_mut(side);
        if accept {
            let return_focus = match panel.mode {
                app_state::PanelMode::Edit(ref edit) => edit.return_focus,
                _ => side,
            };
            panel.mode = app_state::PanelMode::Browser;
            app.active_panel = return_focus;
        } else if let app_state::PanelMode::Edit(ref mut edit) = panel.mode {
            edit.confirm_discard = false;
        }
    }
}

fn pending_op_text(op: &app_state::PendingOp) -> (&'static str, String) {
    match op {
        app_state::PendingOp::Copy { items, dst } => {
            let dst_display = match dst {
                app_state::CopyDest::Local(dir) => dir.to_string_lossy().into_owned(),
                app_state::CopyDest::Remote { host, path } => format!("{host}:{path}"),
            };
            let body = if items.len() == 1 {
                format!(
                    "Copy \"{}\" to\n{}?",
                    items[0].src.display_name(),
                    dst_display
                )
            } else {
                format!("Copy {} items to\n{}?", items.len(), dst_display)
            };
            ("Confirm Copy", body)
        }
        app_state::PendingOp::Move { items, dst } => {
            let dst_display = match dst {
                app_state::CopyDest::Local(dir) => dir.to_string_lossy().into_owned(),
                app_state::CopyDest::Remote { host, path } => format!("{host}:{path}"),
            };
            let body = if items.len() == 1 {
                format!(
                    "Move \"{}\" to\n{}?",
                    items[0].src.display_name(),
                    dst_display
                )
            } else {
                format!("Move {} items to\n{}?", items.len(), dst_display)
            };
            ("Confirm Move", body)
        }
        app_state::PendingOp::Delete { targets } => {
            let body = if targets.len() == 1 {
                format!(
                    "Delete \"{}\"?",
                    targets[0]
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("<unknown>")
                )
            } else {
                format!("Delete {} items?", targets.len())
            };
            ("Confirm Delete", body)
        }
        app_state::PendingOp::Rename { src } => (
            "Rename",
            format!(
                "Rename \"{}\" to:",
                src.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("<unknown>")
            ),
        ),
        app_state::PendingOp::Pack { sources, dst_dir } => {
            let body = if sources.len() == 1 {
                format!(
                    "Pack \"{}\" into archive\n(in {}):",
                    sources[0]
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("<unknown>"),
                    dst_dir.to_string_lossy()
                )
            } else {
                format!(
                    "Pack {} items into archive\n(in {}):",
                    sources.len(),
                    dst_dir.to_string_lossy()
                )
            };
            ("Pack", body)
        }
    }
}
