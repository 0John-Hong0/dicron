//! The 3D workspace: its toolbar control row and its central body.

mod mpr;
mod volume_render;

use eframe::egui;

use crate::app::DicronApp;
use crate::app::state::{ActiveVolumeView, ThreeDMode, ViewMode};
use crate::app::three_d_state::{
    MPR_ISOTROPIC_TARGETS_MM, MprMode, MprQuality, MprState, TransferPreset, VolumeRenderState,
};
use crate::app::volume_job::VolumeJobKind;
use crate::dicom::VolumeGeometry;
use crate::theme;

pub(super) use volume_render::sync_render_state;

/// Why volume rendering is greyed out, shown on hover and reported when it is
/// asked for anyway.
pub(in crate::app) const GPU_UNAVAILABLE_MESSAGE: &str =
    "Volume rendering needs the GPU (wgpu) renderer.";
/// Shown by every mode while the selected series is still being reconstructed,
/// or until its workspace replaces another series' one.
const PREPARING_VOLUME_MESSAGE: &str = "Preparing the 3D volume…";

/// Why the 3D menu is greyed out, shown on hover and reported when a series
/// selected from inside 3D cannot be reconstructed.
pub(in crate::app) const THREE_D_UNSUPPORTED_MESSAGE: &str =
    "3D needs a grayscale series with at least two single-frame images.";

/// The `3D` menu at the head of the 2D viewer's control row: it opens one of
/// the 3D modes for the selected series.
pub(super) fn show_open_menu(
    ui: &mut egui::Ui,
    series_supported: bool,
    gpu_available: bool,
) -> Option<ThreeDMode> {
    if !series_supported {
        ui.add_enabled(false, egui::Button::new("3D"))
            .on_disabled_hover_text(THREE_D_UNSUPPORTED_MESSAGE);
        return None;
    }

    let mut chosen = None;

    egui::containers::menu::MenuButton::from_button(egui::Button::new("3D"))
        .config(egui::containers::menu::MenuConfig::new().style(theme::popup_menu_style))
        .ui(ui, |ui| {
            for mode in ThreeDMode::ALL {
                let available = gpu_available || mode != ThreeDMode::VolumeRendering;
                let response = ui.add_enabled(available, egui::Button::new(mode.label()));
                let response = if available {
                    response
                } else {
                    response.on_disabled_hover_text(GPU_UNAVAILABLE_MESSAGE)
                };

                if response.clicked() {
                    chosen = Some(mode);
                    ui.close();
                }
            }
        });

    chosen
}

pub(super) enum ThreeDControlAction {
    LeaveThreeD,
    SetMode(ThreeDMode),
    ResetView,
    SetQuality(MprQuality),
}

pub(super) fn show_control_row(
    app: &mut DicronApp,
    ui: &mut egui::Ui,
) -> Option<ThreeDControlAction> {
    let active_mode = app.three_d.view_mode.three_d_mode()?;
    let gpu_available = app.three_d.gpu_available;
    let shows_selection = app.workspace_shows_selection();
    let three_d = &mut app.three_d;
    let mut action = None;

    theme::toolbar_row(ui, |ui| {
        if ui
            .button("2D")
            .on_hover_text("Return to the 2D viewer")
            .clicked()
        {
            action = Some(ThreeDControlAction::LeaveThreeD);
        }

        ui.separator();

        for mode in ThreeDMode::ALL {
            let selected = mode == active_mode;

            if mode == ThreeDMode::VolumeRendering && !gpu_available {
                ui.add_enabled(false, egui::Button::selectable(selected, mode.label()))
                    .on_disabled_hover_text(GPU_UNAVAILABLE_MESSAGE);
                continue;
            }

            if ui.selectable_label(selected, mode.label()).clicked() && !selected {
                action = Some(ThreeDControlAction::SetMode(mode));
            }
        }

        ui.separator();

        if ui.button("Reset View").clicked() {
            action = Some(ThreeDControlAction::ResetView);
        }

        // A resample in flight names the quality it is preparing.
        let preparing_quality = three_d
            .job
            .request()
            .and_then(|request| match request.kind {
                VolumeJobKind::Resample { quality, .. } => Some(quality),
                _ => None,
            });
        let job_running = three_d.job.request().is_some();

        // The workspace's own controls wait until it shows the selected series.
        if let Some(active) = three_d.active.as_mut().filter(|_| shows_selection) {
            if active_mode != ThreeDMode::VolumeRendering {
                ui.separator();
                show_window_controls(ui, &mut active.mpr);
                show_quality_menu(ui, active, preparing_quality, job_running, &mut action);
            }

            if active_mode == ThreeDMode::VolumeRendering {
                ui.separator();
                show_transfer_preset_menu(ui, &mut active.render);
                ui.checkbox(&mut active.render.shading, "Shading")
                    .on_hover_text("Light surfaces by the value gradient");

                // Grayscale spans the planes' window, so it is edited here too.
                if active.render.preset == TransferPreset::Grayscale {
                    ui.separator();
                    show_window_controls(ui, &mut active.mpr);
                }
            }

            if active_mode.mpr_mode().is_some_and(MprMode::has_slab) {
                ui.separator();
                show_read_out(
                    ui,
                    mpr::slab_thickness_label(&active.mpr, &active.prepared.series.geometry),
                );
            }

            ui.separator();

            let mut info = egui::text::LayoutJob::default();
            let format = |color| egui::TextFormat {
                font_id: egui::TextStyle::Body.resolve(ui.style()),
                color,
                ..Default::default()
            };
            if active.low_resolution {
                info.append(
                    "Low-resolution source | ",
                    0.0,
                    format(ui.visuals().warn_fg_color),
                );
            }
            info.append(
                &volume_info_text(&active.prepared.series.geometry),
                0.0,
                format(ui.visuals().text_color()),
            );
            show_read_out(ui, info);
        }
    });

    action
}

/// Window level and width for the MPR planes, resettable to the window the
/// series opened with.
///
/// The ranges bound what the user types or drags, never the window itself:
/// a series can open on a wider one, such as a PET measured in Bq/ml, and
/// egui would otherwise clamp it on every frame the fields are shown.
fn show_window_controls(ui: &mut egui::Ui, mpr: &mut MprState) {
    ui.label("WL");
    ui.add(
        egui::DragValue::new(&mut mpr.window.center)
            .range(crate::app::state::WindowLevel::CENTER_RANGE)
            .clamp_existing_to_range(false)
            .speed(1.0),
    );

    ui.label("WW");
    ui.add(
        egui::DragValue::new(&mut mpr.window.width)
            .range(crate::app::state::WindowLevel::WIDTH_RANGE)
            .clamp_existing_to_range(false)
            .speed(1.0),
    );

    if ui
        .button("Reset WL")
        .on_hover_text("Restore the window this series opened with")
        .clicked()
    {
        mpr.reset_window();
    }
}

/// The voxel size the MPR planes are sampled at: the one shown, or the one
/// being prepared. While a 3D job runs, only grids already in memory can be
/// chosen, since another resample would have to wait for it.
fn show_quality_menu(
    ui: &mut egui::Ui,
    active: &ActiveVolumeView,
    preparing: Option<MprQuality>,
    job_running: bool,
    action: &mut Option<ThreeDControlAction>,
) {
    let source = &active.prepared.series.volume;
    let button = egui::Button::new(match preparing {
        Some(quality) => format!("Quality: {} (preparing…)", quality.label(source)),
        None => format!("Quality: {}", active.mpr.quality.label(source)),
    });

    egui::containers::menu::MenuButton::from_button(button)
        .config(egui::containers::menu::MenuConfig::new().style(theme::popup_menu_style))
        .ui(ui, |ui| {
            let choices = [MprQuality::Auto, MprQuality::Original]
                .into_iter()
                .chain(MPR_ISOTROPIC_TARGETS_MM.map(MprQuality::Iso));

            for quality in choices {
                let available = !job_running || active.volume_for_quality(quality).is_some();
                let response = ui.add_enabled(
                    available,
                    egui::Button::selectable(active.mpr.quality == quality, quality.label(source)),
                );

                if response
                    .on_disabled_hover_text(
                        "Available once the 3D preparation in progress finishes",
                    )
                    .clicked()
                {
                    *action = Some(ThreeDControlAction::SetQuality(quality));
                    ui.close();
                }
            }
        });
}

/// The transfer function the volume rendering view maps values through.
fn show_transfer_preset_menu(ui: &mut egui::Ui, render: &mut VolumeRenderState) {
    let button = egui::Button::new(format!("Preset: {}", render.preset.label()));

    egui::containers::menu::MenuButton::from_button(button)
        .config(egui::containers::menu::MenuConfig::new().style(theme::popup_menu_style))
        .ui(ui, |ui| {
            for preset in TransferPreset::ALL {
                if ui
                    .selectable_label(render.preset == preset, preset.label())
                    .clicked()
                {
                    render.preset = preset;
                    ui.close();
                }
            }
        });
}

pub(super) fn show_body(app: &mut DicronApp, ui: &mut egui::Ui) {
    match app.three_d.view_mode {
        ViewMode::ThreeD(mode) => match mode.mpr_mode() {
            Some(mpr_mode) => mpr::show(app, ui, mpr_mode),
            None => volume_render::show(app, ui),
        },
        ViewMode::TwoD => {}
    }
}

/// A message centred in the dark viewer panel, for states with nothing to draw.
pub(super) fn paint_centered_message(ui: &mut egui::Ui, message: &str) {
    let available_size = ui.available_size();
    let (rect, _response) = ui.allocate_exact_size(
        egui::vec2(available_size.x.max(1.0), available_size.y.max(1.0)),
        egui::Sense::hover(),
    );

    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        message,
        egui::FontId::proportional(16.0),
        ui.visuals().text_color(),
    );
}

/// A read-out in the wrapping control row, kept on one line so it moves to the
/// next row whole instead of breaking in the middle of a value.
fn show_read_out(ui: &mut egui::Ui, text: impl Into<egui::WidgetText>) {
    ui.add(egui::Label::new(text).wrap_mode(egui::TextWrapMode::Extend));
}

fn volume_info_text(geometry: &VolumeGeometry) -> String {
    let slice_thickness = geometry
        .slice_thickness_mm
        .map_or_else(|| "-".to_owned(), |thickness| format!("{thickness:.2} mm"));

    format!(
        "{} slices | ST {} | Z {}",
        geometry.slice_count,
        slice_thickness,
        geometry.slice_spacing_text()
    )
}

/// Test support: every text a frame painted, in painting order.
#[cfg(test)]
fn painted_texts(output: &egui::FullOutput) -> Vec<String> {
    fn collect(shape: &egui::Shape, texts: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) => texts.push(text.galley.text().to_owned()),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect(shape, texts);
                }
            }
            _ => {}
        }
    }

    let mut texts = Vec::new();
    for clipped in &output.shapes {
        collect(&clipped.shape, &mut texts);
    }
    texts
}

#[cfg(test)]
mod tests {
    use crate::dicom::Volume;

    use super::*;

    /// A workspace of the selected in-memory series, open in `mode`.
    fn app_in_mode(mode: ThreeDMode, preset: TransferPreset) -> DicronApp {
        use crate::app::state::ActiveVolumeView;
        use crate::app::volume_job::PreparedVolume;
        use crate::dicom::SeriesVolume;

        let volume = std::sync::Arc::new(Volume::new(2, 2, 2, [1.0; 3], vec![0.0; 8]));
        let mut app = DicronApp::default();
        app.index_in_memory_series(2);
        let (series_key, fingerprint) = app.select_in_memory_series(0);
        app.three_d.view_mode = ViewMode::ThreeD(mode);
        app.three_d.active = Some(ActiveVolumeView {
            series_key,
            fingerprint,
            series_overlay: None,
            prepared: PreparedVolume {
                series: std::sync::Arc::new(SeriesVolume {
                    volume: std::sync::Arc::clone(&volume),
                    geometry: geometry(Some(1.0)),
                }),
                mpr: std::sync::Arc::clone(&volume),
                gpu: None,
            },
            low_resolution: false,
            mpr: MprState::new(
                volume,
                crate::app::state::WindowLevel {
                    center: 40.0,
                    width: 400.0,
                },
            ),
            render: VolumeRenderState {
                preset,
                ..VolumeRenderState::default()
            },
            gpu_budget: crate::app::three_d_state::GpuBudget::FULL,
        });

        app
    }

    fn control_row_texts(app: &mut DicronApp) -> Vec<String> {
        let context = egui::Context::default();
        let output = context.run_ui(egui::RawInput::default(), |ui| {
            let _ = show_control_row(app, ui);
        });

        painted_texts(&output)
    }

    #[test]
    fn only_ct_opens_on_a_ct_preset() {
        assert_eq!(
            TransferPreset::for_modality(Some("CT")),
            TransferPreset::CtSoftTissue
        );
        assert_eq!(
            TransferPreset::for_modality(Some(" ct ")),
            TransferPreset::CtSoftTissue
        );
        assert_eq!(
            TransferPreset::for_modality(Some("MR")),
            TransferPreset::Grayscale
        );
        assert_eq!(
            TransferPreset::for_modality(Some("PT")),
            TransferPreset::Grayscale
        );
        assert_eq!(
            TransferPreset::for_modality(None),
            TransferPreset::Grayscale
        );
    }

    /// Grayscale volume rendering spans the window, so the window fields show
    /// with it; the CT presets carry their own Hounsfield scale.
    #[test]
    fn volume_rendering_offers_the_window_with_the_grayscale_preset() {
        let has_window_fields = |texts: &[String]| texts.iter().any(|text| text == "WL");

        let mut grayscale = app_in_mode(ThreeDMode::VolumeRendering, TransferPreset::Grayscale);
        let mut ct = app_in_mode(ThreeDMode::VolumeRendering, TransferPreset::CtSoftTissue);

        assert!(has_window_fields(&control_row_texts(&mut grayscale)));
        assert!(!has_window_fields(&control_row_texts(&mut ct)));
    }

    /// While another series is selected, the controls of the workspace being
    /// replaced are not offered.
    #[test]
    fn the_workspace_controls_wait_for_the_selected_series() {
        let mut app = app_in_mode(ThreeDMode::Mpr, TransferPreset::Grayscale);
        assert!(control_row_texts(&mut app).iter().any(|text| text == "WL"));

        app.select_in_memory_series(1);
        let texts = control_row_texts(&mut app);

        assert!(!texts.iter().any(|text| text == "WL"), "{texts:?}");
        assert!(texts.iter().any(|text| text == "2D"));
    }

    fn geometry(slice_thickness_mm: Option<f64>) -> VolumeGeometry {
        VolumeGeometry {
            inverted: false,
            row_direction: [1.0, 0.0, 0.0],
            column_direction: [0.0, 1.0, 0.0],
            slice_direction: [0.0, 0.0, 1.0],
            slice_thickness_mm,
            slice_spacing_mm: 0.9,
            slice_count: 394,
            uneven_slice_offsets_mm: None,
        }
    }

    /// A window wider than the fields accept survives the frames that show it,
    /// and Reset WL can bring it back.
    #[test]
    fn the_window_fields_never_clamp_the_window_itself() {
        let wide = crate::app::state::WindowLevel {
            center: 300_000.0,
            width: 600_000.0,
        };
        let volume = std::sync::Arc::new(Volume::new(2, 2, 2, [1.0; 3], vec![0.0; 8]));
        let mut mpr = MprState::new(volume, wide);
        let context = egui::Context::default();
        let show = |mpr: &mut MprState| {
            let _ = context.run_ui(egui::RawInput::default(), |ui| {
                show_window_controls(ui, mpr);
            });
        };

        show(&mut mpr);
        assert_eq!(mpr.window, wide);

        mpr.reset_window();
        show(&mut mpr);
        assert_eq!(mpr.window, wide);
    }

    #[test]
    fn the_info_label_states_the_slice_geometry() {
        assert_eq!(
            volume_info_text(&geometry(Some(1.0))),
            "394 slices | ST 1.00 mm | Z 0.90 mm"
        );

        let uneven = VolumeGeometry {
            slice_count: 3,
            uneven_slice_offsets_mm: Some(vec![0.0, 0.9, 2.7]),
            ..geometry(Some(1.0))
        };
        assert_eq!(
            volume_info_text(&uneven),
            "3 slices | ST 1.00 mm | Z 0.90 mm (uneven, up to 1.80 mm)"
        );
    }

    #[test]
    fn a_missing_slice_thickness_is_shown_as_a_dash() {
        assert_eq!(
            volume_info_text(&geometry(None)),
            "394 slices | ST - | Z 0.90 mm"
        );
    }
}
