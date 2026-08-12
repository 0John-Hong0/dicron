//! Application-wide visual rhythm and egui style configuration.

use eframe::egui;

// A four-point spacing scale keeps layout decisions predictable while still
// supporting compact desktop controls.
pub(crate) const SPACE_XXS: f32 = 2.0;
pub(crate) const SPACE_XS: f32 = 4.0;
pub(crate) const SPACE_SM: f32 = 8.0;
pub(crate) const SPACE_MD: f32 = 12.0;
pub(crate) const SPACE_LG: f32 = 16.0;

const SPACE_SM_I8: i8 = SPACE_SM as i8;
const SPACE_MD_I8: i8 = SPACE_MD as i8;

pub(crate) fn configure(context: &egui::Context) {
    context.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(SPACE_SM, SPACE_XS);
        style.spacing.window_margin = egui::Margin::same(SPACE_MD_I8);
        style.spacing.menu_margin = egui::Margin::same(SPACE_MD_I8);
        style.spacing.button_padding = egui::vec2(SPACE_XS, SPACE_XXS);
    });
}

/// Toolbar and inspector panels share one explicit content inset.
pub(crate) fn content_panel_frame(style: &egui::Style) -> egui::Frame {
    egui::Frame::side_top_panel(style)
        .inner_margin(egui::Margin::symmetric(SPACE_MD_I8, SPACE_SM_I8))
}

/// The image canvas uses the smaller inset so screen area remains image-first.
pub(crate) fn viewer_frame(style: &egui::Style) -> egui::Frame {
    egui::Frame::central_panel(style).inner_margin(egui::Margin::same(SPACE_SM_I8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_theme_styles_use_the_application_scale() {
        let context = egui::Context::default();
        configure(&context);

        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let style = context.style_of(theme);

            assert_eq!(style.spacing.item_spacing, egui::vec2(SPACE_SM, SPACE_XS));
            assert_eq!(style.spacing.window_margin, egui::Margin::same(SPACE_MD_I8));
            assert_eq!(style.spacing.menu_margin, egui::Margin::same(SPACE_MD_I8));
            assert_eq!(
                style.spacing.button_padding,
                egui::vec2(SPACE_XS, SPACE_XXS)
            );
        }
    }

    #[test]
    fn application_frames_have_explicit_insets() {
        let style = egui::Style::default();

        assert_eq!(
            content_panel_frame(&style).inner_margin,
            egui::Margin::symmetric(SPACE_MD_I8, SPACE_SM_I8)
        );
        assert_eq!(
            viewer_frame(&style).inner_margin,
            egui::Margin::same(SPACE_SM_I8)
        );
    }
}
