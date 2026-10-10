use std::path::PathBuf;

use egui;

use fileman::{app_state, core, sftp};

use crate::color32;

pub struct QuickJumpResult {
    pub path: PathBuf,
    pub category: app_state::QuickJumpCategory,
    /// Host for a typed remote destination. `None` keeps the panel's current host.
    pub host: Option<String>,
}

pub(crate) struct RemoteHere {
    pub host: String,
    pub path: String,
    pub home: Option<String>,
}

fn category_group(cat: app_state::QuickJumpCategory) -> u8 {
    match cat {
        app_state::QuickJumpCategory::Remote => 0,
        app_state::QuickJumpCategory::Home | app_state::QuickJumpCategory::Mount => 1,
        app_state::QuickJumpCategory::Ssh => 2,
    }
}

pub fn draw_quick_jump(
    ctx: &egui::Context,
    app: &mut app_state::AppState,
) -> Option<QuickJumpResult> {
    // Read theme colors before taking the mutable quick-jump borrow of `app`.
    let colors = app.theme.colors();
    let here = current_remote(app);
    let qj = app.quick_jump_mut()?;

    let screen = ctx.content_rect();
    let overlay_layer = egui::LayerId::new(egui::Order::Middle, "quick_jump_overlay".into());
    ctx.layer_painter(overlay_layer).rect_filled(
        screen,
        egui::CornerRadius::ZERO,
        egui::Color32::from_black_alpha(160),
    );

    let mut result: Option<QuickJumpResult> = None;
    let prev_input = qj.input.clone();

    egui::Window::new("Go To")
        .order(egui::Order::Foreground)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .default_width(400.0)
        .show(ctx, |ui| {
            let text_response = ui.add(
                egui::TextEdit::singleline(&mut qj.input)
                    .desired_width(380.0)
                    .hint_text("Path or host:/path"),
            );
            if qj.focus_input {
                text_response.request_focus();
                qj.focus_input = false;
            }

            // Enter on the text field: navigate to typed path or selected item.
            // Keyboard Enter is handled before the widget sees it; this covers a
            // click that confirms the field.
            if text_response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                result = go_to_from_input(qj, here.as_ref());
            }

            ui.add_space(4.0);

            let row_height = 20.0;
            let separator_height = 5.0;
            let max_rows = 12;
            let visible = qj.filtered.len().min(max_rows);
            let scroll_height = (visible as f32) * row_height + 8.0;

            egui::ScrollArea::vertical()
                .max_height(scroll_height)
                .show(ui, |ui| {
                    let mut prev_group: Option<u8> = None;
                    for (fi, &idx) in qj.filtered.iter().enumerate() {
                        let entry = &qj.entries[idx];
                        let group = category_group(entry.category);

                        // Draw separator between groups
                        if let Some(pg) = prev_group {
                            if pg != group {
                                let (sep_rect, _) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), separator_height),
                                    egui::Sense::hover(),
                                );
                                if ui.is_rect_visible(sep_rect) {
                                    let y = sep_rect.center().y;
                                    ui.painter().line_segment(
                                        [
                                            egui::pos2(sep_rect.left() + 4.0, y),
                                            egui::pos2(sep_rect.right() - 4.0, y),
                                        ],
                                        egui::Stroke::new(
                                            2.0_f32,
                                            color32(colors.panel_border_active),
                                        ),
                                    );
                                }
                            }
                        }
                        prev_group = Some(group);

                        let is_selected = fi == qj.selected;
                        let bg = if is_selected {
                            color32(colors.row_bg_selected_active)
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        let fg = if is_selected {
                            color32(colors.row_fg_selected)
                        } else {
                            color32(colors.row_fg_active)
                        };

                        let text = format!("  {}", entry.label);
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), row_height),
                            egui::Sense::click(),
                        );
                        if ui.is_rect_visible(rect) {
                            if is_selected {
                                ui.painter().rect_filled(rect, 0.0, bg);
                            }
                            ui.painter().text(
                                rect.left_center(),
                                egui::Align2::LEFT_CENTER,
                                &text,
                                egui::FontId::proportional(14.0),
                                fg,
                            );
                        }
                        if is_selected {
                            resp.scroll_to_me(Some(egui::Align::Center));
                        }
                        if resp.clicked() {
                            result = Some(QuickJumpResult {
                                path: entry.path.clone(),
                                category: entry.category,
                                host: None,
                            });
                        }
                    }
                });
        });

    // Refilter if input changed
    if qj.input != prev_input {
        refilter(qj);
    }

    result
}

fn refilter(qj: &mut app_state::QuickJumpState) {
    let query = qj.input.to_lowercase();
    let tokens: Vec<&str> = query.split_whitespace().collect();
    qj.filtered = (0..qj.entries.len())
        .filter(|&i| {
            if tokens.is_empty() {
                return true;
            }
            let label = qj.entries[i].label.to_lowercase();
            let path = qj.entries[i].path.to_string_lossy().to_lowercase();
            tokens.iter().all(|t| label.contains(t) || path.contains(t))
        })
        .collect();
    qj.selected = 0;
}

pub(crate) fn current_remote(app: &app_state::AppState) -> Option<RemoteHere> {
    let browser = app.get_active_panel().browser();
    let core::BrowserMode::Remote { ref host, ref path } = browser.browser_mode else {
        return None;
    };
    let home = app
        .sftp_sessions
        .get(host)
        .and_then(|session| session.try_lock().ok())
        .and_then(|session| session.home_dir.clone());
    Some(RemoteHere {
        host: host.clone(),
        path: path.clone(),
        home,
    })
}

pub(crate) fn go_to_from_input(
    qj: &app_state::QuickJumpState,
    here: Option<&RemoteHere>,
) -> Option<QuickJumpResult> {
    let input = qj.input.trim();
    if let Some(result) = explicit_destination(input, here) {
        return Some(result);
    }
    if !qj.filtered.is_empty() {
        let entry = &qj.entries[qj.filtered[qj.selected]];
        return Some(QuickJumpResult {
            path: entry.path.clone(),
            category: entry.category,
            host: None,
        });
    }
    if input.is_empty() {
        return None;
    }
    if let Some(here) = here {
        return Some(remote_result(
            here.host.clone(),
            resolve_relative(&here.path, input, here.home.as_deref()),
        ));
    }
    Some(local_result(&expand_tilde(input)))
}

fn explicit_destination(input: &str, here: Option<&RemoteHere>) -> Option<QuickJumpResult> {
    if input.is_empty() {
        return None;
    }
    if let Some((host, path)) = parse_sftp_url(input) {
        return Some(remote_result(host, path));
    }
    if let Some(host) = parse_ssh_label(input) {
        return Some(remote_result(host, "/".to_string()));
    }
    if is_windows_path(input) {
        return Some(local_result(input));
    }
    if let Some((host, path)) = parse_host_path(input, here) {
        return Some(remote_result(host, path));
    }
    if input.starts_with('~') {
        if let Some(here) = here {
            return Some(remote_result(
                here.host.clone(),
                sftp::canonicalize_remote_path(input, here.home.as_deref()),
            ));
        }
        return Some(local_result(&expand_tilde(input)));
    }
    if input.starts_with('/') || input.starts_with('\\') {
        if let Some(here) = here {
            return Some(remote_result(
                here.host.clone(),
                sftp::normalize_remote_path(input),
            ));
        }
        return Some(local_result(input));
    }
    if input.contains('/') || input.contains('\\') {
        if let Some(here) = here {
            return Some(remote_result(
                here.host.clone(),
                resolve_relative(&here.path, input, here.home.as_deref()),
            ));
        }
        return Some(local_result(input));
    }
    None
}

fn remote_result(host: String, path: String) -> QuickJumpResult {
    QuickJumpResult {
        path: PathBuf::from(path),
        category: app_state::QuickJumpCategory::Remote,
        host: Some(host),
    }
}

fn local_result(path: &str) -> QuickJumpResult {
    QuickJumpResult {
        path: PathBuf::from(path),
        category: app_state::QuickJumpCategory::Home,
        host: None,
    }
}

fn resolve_relative(dir: &str, input: &str, home: Option<&str>) -> String {
    if input == "~" || input.starts_with("~/") {
        return sftp::canonicalize_remote_path(input, home);
    }
    if input.starts_with('/') {
        return sftp::normalize_remote_path(input);
    }
    let joined = if dir.is_empty() || dir == "/" {
        format!("/{input}")
    } else {
        format!("{}/{input}", dir.trim_end_matches('/'))
    };
    sftp::normalize_remote_path(&joined)
}

fn parse_sftp_url(input: &str) -> Option<(String, String)> {
    let rest = strip_prefix_ignore_ascii_case(input, "sftp://")?;
    let (host, path) = match rest.split_once('/') {
        Some((host, path)) => (host, sftp::normalize_remote_path(&format!("/{path}"))),
        None => (rest, "/".to_string()),
    };
    if !is_host_token(host) {
        return None;
    }
    Some((host.to_string(), path))
}

fn parse_ssh_label(input: &str) -> Option<String> {
    let host = strip_prefix_ignore_ascii_case(input, "ssh:")?.trim();
    if is_host_token(host) {
        Some(host.to_string())
    } else {
        None
    }
}

/// `host:/path`, `host:~`, and `host:dir/child`. A bare `host:2222` is left
/// alone so a port is not read as a path.
fn parse_host_path(input: &str, here: Option<&RemoteHere>) -> Option<(String, String)> {
    let (host, rest) = input.split_once(':')?;
    if !is_host_token(host) {
        return None;
    }
    if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('~') || rest.contains('/')) {
        return None;
    }
    let home = here
        .filter(|here| here.host == host)
        .and_then(|here| here.home.as_deref());
    let path = if rest.is_empty() || rest == "/" {
        "/".to_string()
    } else if rest.starts_with('/') {
        sftp::normalize_remote_path(rest)
    } else if rest == "~" || rest.starts_with("~/") {
        sftp::canonicalize_remote_path(rest, home)
    } else {
        sftp::canonicalize_remote_path(&format!("~/{rest}"), home)
    };
    Some((host.to_string(), path))
}

fn is_host_token(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte))
}

fn is_windows_path(input: &str) -> bool {
    #[cfg(not(windows))]
    {
        let _ = input;
        false
    }
    #[cfg(windows)]
    {
        let bytes = input.as_bytes();
        bytes.len() >= 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes.len() == 2 || bytes[2] == b'\\' || bytes[2] == b'/')
    }
}

fn strip_prefix_ignore_ascii_case<'a>(input: &'a str, prefix: &str) -> Option<&'a str> {
    if input.len() >= prefix.len() && input[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&input[prefix.len()..])
    } else {
        None
    }
}

fn expand_tilde(input: &str) -> String {
    if input.starts_with("~/") || input == "~" {
        let home = {
            #[cfg(windows)]
            {
                std::env::var("USERPROFILE")
                    .or_else(|_| std::env::var("HOME"))
                    .ok()
            }
            #[cfg(not(windows))]
            {
                std::env::var("HOME").ok()
            }
        };
        if let Some(h) = home {
            return input.replacen('~', &h, 1);
        }
    }
    input.to_string()
}

#[cfg(test)]
mod tests {
    use super::{RemoteHere, go_to_from_input};
    use fileman::app_state::{QuickJumpCategory, QuickJumpEntry, QuickJumpState};

    fn here() -> RemoteHere {
        RemoteHere {
            host: "dev".to_string(),
            path: "/var/www".to_string(),
            home: Some("/home/me".to_string()),
        }
    }

    fn state(input: &str, entries: Vec<QuickJumpEntry>) -> QuickJumpState {
        let filtered: Vec<usize> = (0..entries.len()).collect();
        QuickJumpState {
            input: input.to_string(),
            entries,
            filtered,
            selected: 0,
            focus_input: false,
        }
    }

    fn ssh(host: &str) -> QuickJumpEntry {
        QuickJumpEntry {
            label: format!("ssh: {host}"),
            path: std::path::PathBuf::from(host),
            category: QuickJumpCategory::Ssh,
        }
    }

    #[test]
    fn absolute_and_relative_paths_stay_on_the_current_remote() {
        let here = here();
        let abs = go_to_from_input(&state("/etc/nginx", Vec::new()), Some(&here)).unwrap();
        assert_eq!(abs.host.as_deref(), Some("dev"));
        assert_eq!(abs.path, std::path::PathBuf::from("/etc/nginx"));
        assert!(matches!(abs.category, QuickJumpCategory::Remote));

        let rel = go_to_from_input(&state("sites/a", Vec::new()), Some(&here)).unwrap();
        assert_eq!(rel.path, std::path::PathBuf::from("/var/www/sites/a"));

        let home = go_to_from_input(&state("~/src", Vec::new()), Some(&here)).unwrap();
        assert_eq!(home.path, std::path::PathBuf::from("/home/me/src"));

        let up = go_to_from_input(&state("..", Vec::new()), Some(&here)).unwrap();
        assert_eq!(up.path, std::path::PathBuf::from("/var"));
    }

    #[test]
    fn host_prefix_opens_that_host_even_when_it_is_not_listed() {
        let here = here();
        let typed =
            go_to_from_input(&state("other:/var/log", vec![ssh("dev")]), Some(&here)).unwrap();
        assert_eq!(typed.host.as_deref(), Some("other"));
        assert_eq!(typed.path, std::path::PathBuf::from("/var/log"));
        assert!(matches!(typed.category, QuickJumpCategory::Remote));

        let url = go_to_from_input(&state("sftp://files.example/data", Vec::new()), None).unwrap();
        assert_eq!(url.host.as_deref(), Some("files.example"));
        assert_eq!(url.path, std::path::PathBuf::from("/data"));

        let home = go_to_from_input(&state("dev:~", Vec::new()), Some(&here)).unwrap();
        assert_eq!(home.host.as_deref(), Some("dev"));
        assert_eq!(home.path, std::path::PathBuf::from("/home/me"));
    }

    #[test]
    fn a_short_filter_still_selects_the_highlighted_row() {
        let picked = go_to_from_input(&state("dev", vec![ssh("dev")]), Some(&here())).unwrap();
        assert!(matches!(picked.category, QuickJumpCategory::Ssh));
        assert_eq!(picked.path, std::path::PathBuf::from("dev"));
        assert!(picked.host.is_none());
    }

    #[test]
    fn a_port_like_suffix_is_not_a_remote_path() {
        let picked = go_to_from_input(&state("dev:2222", vec![ssh("dev")]), Some(&here())).unwrap();
        assert!(matches!(picked.category, QuickJumpCategory::Ssh));
    }
}
