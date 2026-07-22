//! Scan progress and dismissible warning/error status rendering.

use std::time::Duration;

use eframe::egui;

use crate::app::background_tasks::ScanProgress;

pub(super) fn show_scan_status(ui: &mut egui::Ui, scan_state: Option<&ScanProgress>) {
    let Some(scan_state) = scan_state else {
        return;
    };

    ui.separator();
    ui.label(format!("Scanning: {}", scan_state.source_label));

    let progress = if scan_state.total_file_count == 0 {
        0.0
    } else {
        scan_state.processed_file_count as f32 / scan_state.total_file_count as f32
    };

    let eta_text = estimate_scan_eta(scan_state)
        .map(format_duration)
        .unwrap_or_else(|| "calculating...".to_owned());

    ui.add(
        egui::ProgressBar::new(progress)
            .show_percentage()
            .text(format!(
                "Scanning {} / {} files | {} DICOM | ETA {}",
                scan_state.processed_file_count,
                scan_state.total_file_count,
                scan_state.readable_dicom_count,
                eta_text
            )),
    );
}

pub(super) fn show_error_status(ui: &mut egui::Ui, error_message: Option<&str>) -> bool {
    let Some(error_message) = error_message else {
        return false;
    };

    ui.separator();
    let mut dismiss = false;
    ui.horizontal(|ui| {
        if ui.button("✕").on_hover_text("Dismiss error").clicked() {
            dismiss = true;
        }

        if !dismiss {
            ui.colored_label(egui::Color32::RED, error_message);
        }
    });

    dismiss
}

fn estimate_scan_eta(scan_state: &ScanProgress) -> Option<Duration> {
    if scan_state.processed_file_count == 0 || scan_state.total_file_count == 0 {
        return None;
    }

    let elapsed_seconds = scan_state.started_at.elapsed().as_secs_f64();
    let seconds_per_file = elapsed_seconds / scan_state.processed_file_count as f64;
    let remaining_file_count = scan_state
        .total_file_count
        .saturating_sub(scan_state.processed_file_count);

    Some(Duration::from_secs_f64(
        seconds_per_file * remaining_file_count as f64,
    ))
}

fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::format_duration;

    #[test]
    fn format_duration_rolls_over_to_hours() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_duration(Duration::from_secs(3661)), "1h 1m 1s");
        assert_eq!(format_duration(Duration::from_secs(7200)), "2h 0m 0s");
    }
}
