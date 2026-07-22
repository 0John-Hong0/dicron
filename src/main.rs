#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;
mod dicom;
mod metadata_table;
mod release_check;
mod settings;

use std::path::PathBuf;
use std::sync::Arc;

use app::DicronApp;
use eframe::egui::{self, FontData, FontDefinitions, FontFamily, FontTweak};
#[cfg(target_os = "linux")]
use winit::platform::x11::EventLoopBuilderExtX11;

fn main() -> eframe::Result {
    let startup_paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Dicron")
            .with_inner_size([1200.0, 760.0])
            .with_min_inner_size([900.0, 600.0])
            .with_app_id("dicron")
            .with_icon(load_window_icon()),
        #[cfg(target_os = "linux")]
        event_loop_builder: Some(Box::new(|event_loop_builder| {
            // winit 0.30 receives file drops on X11, but not on its Wayland backend.
            event_loop_builder.with_x11();
        })),
        ..Default::default()
    };

    eframe::run_native(
        "dicron",
        options,
        Box::new(move |creation_context| {
            configure_fonts(&creation_context.egui_ctx);

            let mut app = DicronApp::default();

            if !startup_paths.is_empty() {
                app.open_startup_paths(&creation_context.egui_ctx, startup_paths.clone());
            }

            Ok(Box::new(app))
        }),
    )
}

fn load_window_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
        .unwrap_or_else(|_| egui::IconData::default())
}

fn configure_fonts(context: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    add_primary_font(
        &mut fonts,
        "geist_regular",
        include_bytes!("../assets/fonts/Geist-Regular.ttf"),
        FontFamily::Proportional,
    );

    add_primary_font(
        &mut fonts,
        "geist_mono_regular",
        include_bytes!("../assets/fonts/GeistMono-Regular.ttf"),
        FontFamily::Monospace,
    );

    add_cjk_fallback_font(
        &mut fonts,
        "source_han_sans_cjk",
        include_bytes!("../assets/fonts/SourceHanSans.ttc"),
        25,
    );

    context.set_fonts(fonts);
}

fn add_primary_font(
    fonts: &mut FontDefinitions,
    font_name: &str,
    font_bytes: &'static [u8],
    font_family: FontFamily,
) {
    let font_name = font_name.to_owned();

    fonts.font_data.insert(
        font_name.clone(),
        Arc::new(FontData::from_static(font_bytes)),
    );

    if let Some(fonts_for_family) = fonts.families.get_mut(&font_family) {
        fonts_for_family.insert(0, font_name);
    }
}

fn add_cjk_fallback_font(
    fonts: &mut FontDefinitions,
    font_name: &str,
    font_bytes: &'static [u8],
    font_index: u32,
) {
    // epaint parses the face lazily on the first repaint and panics on an
    // out-of-range collection index. Validate the index up front so a swapped
    // or unexpected font file degrades to "no CJK fallback" instead of a crash.
    if font_index >= ttc_face_count(font_bytes) {
        return;
    }

    let font_name = font_name.to_owned();

    let mut font_data = FontData::from_static(font_bytes);
    font_data.index = font_index;

    // Source Han Sans has different metrics from Geist.
    // Scale/offset keeps CJK fallback text visually aligned with Geist.
    font_data.tweak = FontTweak {
        scale: 0.90,
        y_offset_factor: -0.08,
        ..Default::default()
    };

    fonts
        .font_data
        .insert(font_name.clone(), Arc::new(font_data));

    if let Some(proportional_fonts) = fonts.families.get_mut(&FontFamily::Proportional) {
        proportional_fonts.push(font_name.clone());
    }

    if let Some(monospace_fonts) = fonts.families.get_mut(&FontFamily::Monospace) {
        monospace_fonts.push(font_name);
    }
}

/// Number of faces in a font file: the `numFonts` field of a TrueType
/// Collection (`ttcf`), or 1 for a single-face sfnt. Returns 0 if the bytes
/// are not a recognizable font, so any positive index is rejected.
fn ttc_face_count(font_bytes: &[u8]) -> u32 {
    if font_bytes.len() < 12 {
        return 0;
    }

    if &font_bytes[0..4] == b"ttcf" {
        return u32::from_be_bytes([font_bytes[8], font_bytes[9], font_bytes[10], font_bytes[11]]);
    }

    let sfnt_version = &font_bytes[0..4];
    let is_single_face = sfnt_version == [0x00, 0x01, 0x00, 0x00]
        || sfnt_version == b"OTTO"
        || sfnt_version == b"true"
        || sfnt_version == b"typ1";

    if is_single_face { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttc_collection_reports_num_fonts() {
        let collection = [b't', b't', b'c', b'f', 1, 0, 0, 0, 0, 0, 0, 3];
        assert_eq!(ttc_face_count(&collection), 3);
    }

    #[test]
    fn single_face_fonts_report_one() {
        let truetype = [0x00, 0x01, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0];
        let opentype = [b'O', b'T', b'T', b'O', 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(ttc_face_count(&truetype), 1);
        assert_eq!(ttc_face_count(&opentype), 1);
    }

    #[test]
    fn unrecognized_or_short_bytes_report_zero() {
        assert_eq!(ttc_face_count(&[0, 1, 2]), 0);
        assert_eq!(ttc_face_count(&[0xFF; 12]), 0);
    }
}
