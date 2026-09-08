//! Compact one-card-per-series presentation for the DICOM tree.

use eframe::egui;

use super::hierarchy::paint_truncated_text;
use crate::app::series_thumbnail_cache::SeriesThumbnailCache;
use crate::app::state::{SeriesKey, SliceSelection};
use crate::dicom::SliceItem;
use crate::theme;

const SERIES_PREVIEW_HEIGHT: f32 = 88.0;
const SERIES_PREVIEW_INSET: f32 = theme::SPACE_SM;

pub(super) trait ThumbnailProvider {
    fn request(&mut self, context: &egui::Context, key: SeriesKey, slices: &[SliceItem]);
    fn texture(&self, key: SeriesKey) -> Option<&egui::TextureHandle>;
    fn failed(&self, key: SeriesKey) -> bool;
}

impl ThumbnailProvider for SeriesThumbnailCache {
    fn request(&mut self, context: &egui::Context, key: SeriesKey, slices: &[SliceItem]) {
        SeriesThumbnailCache::request(self, context, key, slices);
    }

    fn texture(&self, key: SeriesKey) -> Option<&egui::TextureHandle> {
        SeriesThumbnailCache::texture(self, key)
    }

    fn failed(&self, key: SeriesKey) -> bool {
        SeriesThumbnailCache::failed(self, key)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn show_series_preview(
    ui: &mut egui::Ui,
    series_description: Option<&str>,
    series_number: Option<i32>,
    modality: Option<&str>,
    slices: &[SliceItem],
    series_key: SeriesKey,
    selected_selection: Option<SliceSelection>,
    last_slice_index: Option<usize>,
    thumbnails: &mut impl ThumbnailProvider,
    clicked_selection: &mut Option<SliceSelection>,
) {
    if slices.is_empty() {
        return;
    }

    let selected_slice_index = selected_selection
        .filter(|selection| selection.series_key() == series_key)
        .map(|selection| selection.slice_index);
    let is_selected = selected_slice_index.is_some();
    let open_slice_index =
        series_open_slice_index(selected_slice_index, last_slice_index, slices.len());
    let selection = SliceSelection::new(series_key.0, series_key.1, series_key.2, open_slice_index);
    let series_title = series_title_text(modality, series_description);
    let series_number = series_number_text(series_number);
    let slice_count = slice_count_text(slices.len());

    let card_size = egui::vec2(ui.available_width(), SERIES_PREVIEW_HEIGHT);
    let (card_rect, mut response) = ui.allocate_exact_size(card_size, egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            is_selected,
            &series_title,
        )
    });

    if ui.is_rect_visible(card_rect) {
        thumbnails.request(ui.ctx(), series_key, slices);

        let visuals = ui.visuals();
        let card_fill = if is_selected {
            visuals
                .selection
                .bg_fill
                .lerp_to_gamma(visuals.panel_fill, 0.55)
        } else if response.hovered() {
            visuals.widgets.hovered.weak_bg_fill
        } else {
            visuals.faint_bg_color
        };
        ui.painter()
            .rect_filled(card_rect, theme::SPACE_XS, card_fill);

        let content_rect = card_rect.shrink(SERIES_PREVIEW_INSET);
        let thumbnail_size = content_rect.height();
        let thumbnail_rect =
            egui::Rect::from_min_size(content_rect.min, egui::vec2(thumbnail_size, thumbnail_size));
        ui.painter()
            .rect_filled(thumbnail_rect, theme::SPACE_XXS, egui::Color32::BLACK);

        if let Some(texture) = thumbnails.texture(series_key) {
            let image_rect = fit_texture_inside(texture.size_vec2(), thumbnail_rect);
            ui.painter().image(
                texture.id(),
                image_rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else {
            let status = if thumbnails.failed(series_key) {
                "No preview"
            } else {
                "Loading…"
            };
            ui.painter().text(
                thumbnail_rect.center(),
                egui::Align2::CENTER_CENTER,
                status,
                egui::TextStyle::Small.resolve(ui.style()),
                visuals.weak_text_color(),
            );
        }

        let details_left = thumbnail_rect.right() + theme::SPACE_MD;
        let details_right = content_rect.right();
        let details_width = (details_right - details_left).max(0.0);
        let title_rect = egui::Rect::from_min_size(
            egui::pos2(details_left, content_rect.top() + theme::SPACE_XS),
            egui::vec2(details_width, 18.0),
        );
        let title_was_elided = paint_truncated_text(
            ui,
            title_rect,
            details_left,
            egui::RichText::new(&series_title).strong(),
            egui::TextStyle::Button,
            visuals.strong_text_color(),
        );

        let number_rect = title_rect.translate(egui::vec2(0.0, 22.0));
        paint_truncated_text(
            ui,
            number_rect,
            details_left,
            egui::RichText::new(&series_number).color(visuals.text_color()),
            egui::TextStyle::Body,
            visuals.text_color(),
        );

        let slice_count_rect = title_rect.translate(egui::vec2(0.0, 44.0));
        paint_truncated_text(
            ui,
            slice_count_rect,
            details_left,
            egui::RichText::new(&slice_count).color(visuals.text_color()),
            egui::TextStyle::Body,
            visuals.text_color(),
        );

        if title_was_elided {
            response = response.on_hover_text(&series_title);
        }
    }

    response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        *clicked_selection = Some(selection);
    }
}

fn fit_texture_inside(texture_size: egui::Vec2, bounds: egui::Rect) -> egui::Rect {
    if texture_size.x <= 0.0 || texture_size.y <= 0.0 {
        return bounds;
    }

    let scale = (bounds.width() / texture_size.x)
        .min(bounds.height() / texture_size.y)
        .max(0.0);
    egui::Rect::from_center_size(bounds.center(), texture_size * scale)
}

fn slice_count_label(slice_count: usize) -> &'static str {
    if slice_count == 1 { "slice" } else { "slices" }
}

fn series_title_text(modality: Option<&str>, description: Option<&str>) -> String {
    let description = description
        .filter(|description| !description.is_empty())
        .unwrap_or("Unknown Series");
    match modality.filter(|modality| !modality.is_empty()) {
        Some(modality) => format!("{modality} - {description}"),
        None => description.to_owned(),
    }
}

fn series_number_text(series_number: Option<i32>) -> String {
    series_number
        .map(|number| format!("Series {number}"))
        .unwrap_or_else(|| "Series number unavailable".to_owned())
}

fn slice_count_text(slice_count: usize) -> String {
    format!("{slice_count} {}", slice_count_label(slice_count))
}

fn series_open_slice_index(
    selected_slice_index: Option<usize>,
    last_slice_index: Option<usize>,
    slice_count: usize,
) -> usize {
    selected_slice_index
        .or(last_slice_index)
        .filter(|slice_index| *slice_index < slice_count)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ui::dicom_tree::hierarchy::TREE_ROW_GAP;
    use crate::app::ui::dicom_tree::test_support::{
        id_change_warning_count, painted_texts, run_frame, test_slices,
    };

    #[derive(Default)]
    struct NoopThumbnails;

    impl ThumbnailProvider for NoopThumbnails {
        fn request(&mut self, _: &egui::Context, _: SeriesKey, _: &[SliceItem]) {}

        fn texture(&self, _: SeriesKey) -> Option<&egui::TextureHandle> {
            None
        }

        fn failed(&self, _: SeriesKey) -> bool {
            false
        }
    }

    #[test]
    fn series_cards_open_at_start_then_resume_the_last_slice() {
        assert_eq!(series_open_slice_index(None, None, 50), 0);
        assert_eq!(series_open_slice_index(None, Some(17), 50), 17);
        assert_eq!(series_open_slice_index(Some(23), Some(17), 50), 23);
        assert_eq!(series_open_slice_index(None, Some(50), 50), 0);
    }

    #[test]
    fn series_details_are_split_into_title_number_and_count() {
        assert_eq!(
            series_title_text(Some("CT"), Some("CTA HEAD")),
            "CT - CTA HEAD"
        );
        assert_eq!(series_number_text(Some(12)), "Series 12");
        assert_eq!(slice_count_text(80), "80 slices");
        assert_eq!(series_title_text(None, None), "Unknown Series");
        assert_eq!(series_number_text(None), "Series number unavailable");
    }

    fn show_single_card(
        context: &egui::Context,
        slices: &[SliceItem],
        selected: Option<SliceSelection>,
    ) -> Vec<(String, egui::Rect)> {
        let mut thumbnails = NoopThumbnails;
        let output = run_frame(context, |ui| {
            let mut clicked_selection = None;
            show_series_preview(
                ui,
                Some("CTA HEAD"),
                Some(4),
                Some("CT"),
                slices,
                (0, 0, 0),
                selected,
                None,
                &mut thumbnails,
                &mut clicked_selection,
            );
        });

        painted_texts(&output.shapes)
    }

    #[test]
    fn a_series_card_names_the_series_above_its_summary() {
        let context = egui::Context::default();
        let slices = test_slices(394);

        let texts = show_single_card(&context, &slices, None);
        let line_of = |needle: &str| {
            texts
                .iter()
                .find(|(text, _)| text.starts_with(needle))
                .unwrap_or_else(|| panic!("no line starting with {needle:?} in {texts:?}"))
                .1
        };
        let title = line_of("CT - CTA HEAD");
        let number = line_of("Series 4");
        let count = line_of("394 slices");

        assert!(
            texts.iter().any(|(text, _)| text == "394 slices"),
            "{texts:?}"
        );
        assert!(title.bottom() <= number.top(), "{title:?} {number:?}");
        assert!(number.bottom() <= count.top(), "{number:?} {count:?}");
        assert!(
            count.bottom() - title.top() <= SERIES_PREVIEW_HEIGHT,
            "{title:?} {count:?}"
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn scrolling_series_preview_cards_keeps_widget_ids_stable() {
        let slices = test_slices(40);
        let context = egui::Context::default();
        let mut thumbnails = NoopThumbnails;
        let show_scrolled_cards = |thumbnails: &mut NoopThumbnails, scroll_offset: f32| {
            run_frame(&context, |ui| {
                ui.spacing_mut().item_spacing.y = TREE_ROW_GAP;
                egui::ScrollArea::vertical()
                    .vertical_scroll_offset(scroll_offset)
                    .show(ui, |ui| {
                        let mut clicked_selection = None;
                        for series_index in 0..30 {
                            show_series_preview(
                                ui,
                                Some("CTA HEAD"),
                                Some(1),
                                Some("CT"),
                                &slices,
                                (0, 0, series_index),
                                None,
                                None,
                                thumbnails,
                                &mut clicked_selection,
                            );
                        }
                    });
            })
        };

        show_scrolled_cards(&mut thumbnails, 0.0);

        let card_stride = SERIES_PREVIEW_HEIGHT + TREE_ROW_GAP;
        for scroll_offset in [
            card_stride,
            3.0 * card_stride,
            3.0 * card_stride + 7.0,
            9.0 * card_stride,
            card_stride,
            0.0,
        ] {
            let output = show_scrolled_cards(&mut thumbnails, scroll_offset);

            assert_eq!(
                id_change_warning_count(&output.shapes),
                0,
                "scroll offset {scroll_offset}"
            );
        }
    }
}
