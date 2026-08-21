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
const SPACE_XS_I8: i8 = SPACE_XS as i8;

fn button_padding() -> egui::Vec2 {
    egui::vec2(SPACE_XS, SPACE_XXS)
}

pub(crate) fn configure(context: &egui::Context) {
    context.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(SPACE_SM, SPACE_XS);
        style.spacing.window_margin = egui::Margin::same(SPACE_MD_I8);
        style.spacing.menu_margin = egui::Margin::same(SPACE_MD_I8);
        style.spacing.button_padding = button_padding();
    });
}

/// Keep popup choices compact while retaining the application's button padding.
pub(crate) fn popup_menu_style(style: &mut egui::Style) {
    egui::containers::menu::menu_style(style);
    style.spacing.button_padding = button_padding();
}

/// Toolbar rows use the shared control padding and wrap gaps explicitly.
pub(crate) fn toolbar_row<R>(
    ui: &mut egui::Ui,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    ui.spacing_mut().item_spacing = egui::vec2(SPACE_XS, SPACE_XS);
    ui.spacing_mut().button_padding = button_padding();
    ui.horizontal_wrapped(add_contents)
}

/// The multi-row toolbar is denser vertically than inspector panels.
pub(crate) fn toolbar_panel_frame(style: &egui::Style) -> egui::Frame {
    egui::Frame::side_top_panel(style)
        .inner_margin(egui::Margin::symmetric(SPACE_MD_I8, SPACE_XS_I8))
}

/// Toolbar and inspector panels share one explicit content inset.
pub(crate) fn content_panel_frame(style: &egui::Style) -> egui::Frame {
    egui::Frame::side_top_panel(style)
        .inner_margin(egui::Margin::symmetric(SPACE_MD_I8, SPACE_SM_I8))
}

/// The image canvas uses the smaller inset so screen area remains image-first.
pub(crate) fn viewer_frame(style: &egui::Style) -> egui::Frame {
    egui::Frame::central_panel(style)
        .fill(egui::Color32::BLACK)
        .inner_margin(egui::Margin {
            left: SPACE_SM_I8,
            right: 0,
            top: SPACE_SM_I8,
            bottom: SPACE_SM_I8,
        })
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
            assert_eq!(style.spacing.button_padding, button_padding());
        }
    }

    #[test]
    fn popup_menus_retain_application_button_padding() {
        let mut style = egui::Style::default();
        popup_menu_style(&mut style);

        assert_eq!(style.spacing.button_padding, button_padding());
    }

    #[test]
    fn toolbar_rows_use_application_control_spacing() {
        let context = egui::Context::default();

        let _ = context.run_ui(Default::default(), |ui| {
            toolbar_row(ui, |ui| {
                assert_eq!(ui.spacing().item_spacing, egui::vec2(SPACE_XS, SPACE_XS));
                assert_eq!(ui.spacing().button_padding, button_padding());
            });
        });
    }

    #[test]
    fn application_frames_have_explicit_insets() {
        let style = egui::Style::default();

        assert_eq!(
            content_panel_frame(&style).inner_margin,
            egui::Margin::symmetric(SPACE_MD_I8, SPACE_SM_I8)
        );
        assert_eq!(
            toolbar_panel_frame(&style).inner_margin,
            egui::Margin::symmetric(SPACE_MD_I8, SPACE_XS_I8)
        );
        assert_eq!(
            viewer_frame(&style).inner_margin,
            egui::Margin {
                left: SPACE_SM_I8,
                right: 0,
                top: SPACE_SM_I8,
                bottom: SPACE_SM_I8,
            }
        );
        assert_eq!(viewer_frame(&style).fill, egui::Color32::BLACK);
    }
}
