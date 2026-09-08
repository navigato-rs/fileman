use crate::{Info, Payload, Report, SUPPORT};

/// Lives inside About/help; does not reserve workspace or toolbar space.
pub fn show(ui: &mut egui::Ui, info: Info) {
    ui.collapsing("Feedback and diagnostics", |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.hyperlink_to("Send feedback…", info.issue_url());
            if ui.button("Copy issue link").clicked() { ui.ctx().copy_text(info.issue_url()); }
        });
        ui.weak("GitHub issues are public. Review the draft before submitting.");
        ui.weak("Only app/build, OS and architecture are prefilled. No logs or paths.");
        let private_body = format!("Describe the private issue here.\n\n{}", info.environment());
        private_link(ui, info, &private_body);
        let Some(support) = SUPPORT.get() else {
            ui.weak("Diagnostics and statistics are disabled in demo/replay runs.");
            return;
        };
        let Ok(mut state) = support.try_lock() else { return };
        if state.storage.is_none() {
            ui.weak("Local reporting is unavailable in this run.");
            return;
        }
        ui.separator();
        let mut preferences = state.preferences;
        let diagnostics = ui.checkbox(&mut preferences.diagnostics, "Keep local failure reports").changed();
        ui.weak("Ask before sharing. No automatic uploads or memory dumps.");
        let backtraces = ui.add_enabled(preferences.diagnostics,
            egui::Checkbox::new(&mut preferences.backtraces, "Include application backtraces locally")).changed();
        ui.weak("Off by default. Executable debug ID and code offsets only; no memory or loaded-library paths.");
        let usage = ui.checkbox(&mut preferences.usage, "Collect local usage statistics").changed();
        ui.weak("Off by default. Fixed feature flags and timing buckets; no installation ID.");
        if diagnostics || backtraces || usage { state.save_preferences(preferences); }
        ui.weak("Turning a category off clears its reports; turning backtraces off clears failure reports. Up to 8 local reports. Old reports are pruned on launch and when saving.");
        if preferences.usage && ui.button("Review usage summary").clicked() {
            let report = Report::new(info, Payload::Usage { counters: state.usage.clone() });
            if let Some(ref storage) = state.storage {
                match storage.save(&report) {
                    Ok(path) => {
                        state.reports.insert(0, (path, report));
                        state.reports.truncate(crate::storage::MAX_REPORTS);
                        state.selected = 0;
                    }
                    Err(_) => state.notice = Some("Could not save the usage summary."),
                }
            }
        }
        if !state.reports.is_empty() {
            ui.separator();
            state.selected = state.selected.min(state.reports.len() - 1);
            ui.horizontal_wrapped(|ui| {
                ui.label("Local report");
                let count = state.reports.len();
                if ui.add(egui::DragValue::new(&mut state.selected).range(0..=count-1)).changed() { state.sentry_review = None; }
                ui.weak(format!("of {count} (0 is newest)"));
            });
            let (path, mut report) = state.reports[state.selected].clone();
            if matches!(report.payload, Payload::Failure { trace: Some(_), .. }) {
                let mut approved = state.sentry_review.as_ref() == Some(&path);
                if ui.checkbox(&mut approved, "Allow Sentry processing of this report").changed() {
                    state.sentry_review = approved.then(|| path.clone());
                }
                report.sentry_consent = approved;
                ui.weak("Applies only to the report you copy or email. No automatic upload; Sentry is a third-party service.");
            }
            let mut text = report.text();
            egui::ScrollArea::vertical().id_salt("report-preview").max_height(160.0).show(ui, |ui| {
                ui.add(egui::TextEdit::multiline(&mut text).code_editor().interactive(false).desired_width(f32::INFINITY));
            });
            ui.weak("Review this exact payload. Email also discloses your sender address to the recipient.");
            ui.horizontal_wrapped(|ui| {
                if ui.button("Copy report").clicked() { ui.ctx().copy_text(text.clone()); }
                if let Some(url) = info.email_url(&text) { ui.hyperlink_to("Email report…", url); }
                else { ui.weak("Copy and paste into the private email draft."); }
                if ui.button("Delete report").clicked() {
                    if state.storage.as_ref().unwrap().delete(&path).is_ok() {
                        let index = state.selected; state.reports.remove(index);
                    } else { state.notice = Some("Could not delete the local report."); }
                }
            });
        }
        if let Some(notice) = state.notice { ui.weak(notice); }
        ui.weak("This build never sends reports or statistics automatically.");
    });
}

fn private_link(ui: &mut egui::Ui, info: Info, body: &str) {
    if let Some(url) = info.email_url(body) {
        ui.hyperlink_to("Private report by email…", url);
        ui.weak("Opens your mail application; nothing is sent until you send it.");
    } else {
        ui.add_enabled(false, egui::Button::new("Private report by email…"))
            .on_disabled_hover_text(
                "The release maintainer has not configured a private reporting address.",
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drawing_feedback_does_not_open_a_url_without_a_click() {
        let context = egui::Context::default();
        let output = context.run_ui(egui::RawInput::default(), |ui| {
            show(
                ui,
                Info {
                    app: crate::App::Starcom,
                    version: "0.2.0",
                    revision: None,
                    private_email: None,
                },
            );
        });
        assert!(output.platform_output.commands.is_empty());
    }
}
