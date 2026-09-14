use fileman::{app_state, theme};

use crate::color32;

pub fn draw_command_bar(ui: &mut egui::Ui, app: &app_state::AppState, colors: &theme::ThemeColors) {
    let modifiers = ui.ctx().input(|i| i.modifiers);
    let preview_side = app.preview_panel_side();
    let other_panel_preview = preview_side
        .as_ref()
        .is_some_and(|side| *side != app.active_panel);
    // On macOS, show Ctrl+letter combos instead of F-keys (laptops lack a real F-key row).
    let use_ctrl = cfg!(target_os = "macos");
    // Include label padding; the default Panel frame's margins made exact_size(30)
    // shorter than the key caps, which then painted over the pane above.
    let bar_height = (ui.text_style_height(&egui::TextStyle::Body) + 14.0).max(30.0);
    egui::Panel::bottom("command_bar")
        .frame(egui::Frame::NONE.fill(color32(colors.footer_bg)))
        .exact_size(bar_height)
        .show_inside(ui, |ui| {
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        draw_refresh_indicator(ui, app, colors);
                        macro_rules! k {
                            ($fkey:expr, $ctrl:expr) => {
                                if use_ctrl { $ctrl } else { $fkey }
                            };
                        }
                        draw_key_cap(ui, k!("F1", "^H"), "Help", colors);
                        draw_key_cap(ui, k!("F2", "^R"), "Refresh", colors);
                        let (mut f3, f4, mut f5, mut f6, f7, f8) = if modifiers.alt {
                            ("", "", "Pack", "Unpack", "Search", "")
                        } else if modifiers.shift {
                            ("", "New", "Copy", "Rename", "", "")
                        } else {
                            ("View", "Edit", "Copy", "Move", "Mkdir", "Delete")
                        };
                        if preview_side.is_some() && !modifiers.alt && !modifiers.shift {
                            f3 = "Exit";
                        }
                        if other_panel_preview {
                            if !f5.is_empty() {
                                f5 = "";
                            }
                            if !f6.is_empty() {
                                f6 = "";
                            }
                            if modifiers.shift {
                                f5 = "";
                            }
                        }
                        draw_key_cap(ui, k!("F3", "^P"), f3, colors);
                        if modifiers.shift {
                            draw_key_cap(ui, k!("F4", "^N"), f4, colors);
                        } else {
                            draw_key_cap(ui, k!("F4", "^E"), f4, colors);
                        }
                        if modifiers.alt {
                            draw_key_cap(ui, k!("F5", "^A"), f5, colors);
                        } else {
                            draw_key_cap(ui, k!("F5", "^C"), f5, colors);
                        }
                        if modifiers.shift {
                            draw_key_cap(ui, k!("F6", "^⇧M"), f6, colors);
                        } else {
                            draw_key_cap(ui, k!("F6", "^M"), f6, colors);
                        }
                        if modifiers.alt {
                            draw_key_cap(ui, k!("F7", "^G"), f7, colors);
                        } else {
                            draw_key_cap(ui, k!("F7", "^D"), f7, colors);
                        }
                        draw_key_cap(ui, k!("F8", "^X"), f8, colors);
                    });
                });
        });
}

fn draw_refresh_indicator(
    ui: &mut egui::Ui,
    app: &app_state::AppState,
    colors: &theme::ThemeColors,
) {
    let spinner = ["|", "/", "-", "\\"];
    let symbol = spinner[(app.refresh_tick as usize) % spinner.len()];
    let text = egui::RichText::new(symbol)
        .monospace()
        .color(color32(colors.footer_fg));
    egui::Frame::NONE
        .fill(color32(colors.preview_header_bg))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(text);
        });
    ui.add_space(10.0);
}

fn draw_key_cap(ui: &mut egui::Ui, key: &str, label: &str, colors: &theme::ThemeColors) {
    let key_text = egui::RichText::new(key)
        .color(color32(colors.row_fg_selected))
        .strong();
    egui::Frame::NONE
        .fill(color32(colors.preview_header_bg))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(key_text);
                if !label.is_empty() {
                    let label_text =
                        egui::RichText::new(format!(" {label}")).color(color32(colors.footer_fg));
                    ui.label(label_text);
                }
            });
        });
    ui.add_space(6.0);
}
