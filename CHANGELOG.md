# Changelog

All notable user-facing changes should be recorded here.

This project uses semantic versioning while it remains practical for a small desktop app.

## [Unreleased]

- Add independently collapsible and resizable DICOM tree and metadata side panels.
- Improve spacing and responsive layout throughout the viewer, including controls that wrap at narrow window widths.
- Refine the DICOM tree with clearer hierarchy, compact slice rows, full-row selection, and tooltips for truncated labels.
- Make the metadata table adapt to the available width without horizontal scrolling, with full text on hover and right-click copy actions.
- Add a persistent System, Light, and Dark theme selector styled consistently with the toolbar.
- Fix opening DICOM images encoded with the JPEG 2000 Lossless transfer syntax.
- Add viewer window presets, exact window-level editing, and consistent number-key shortcuts.
- Add right-drag zoom, middle-drag pan, and double-click image fitting.
- Add viewer flip, rotate, and reset-view controls.
- Expand image overlays with patient, study, series, orientation, slice, pixel, and windowing information.
- Keep slice and frame changes transactional so failed loads preserve the currently displayed image and navigation state.
- Exclude non-image DICOM objects from the image hierarchy and accept parseable extensionless or markerless DICOM datasets.
- Store settings in the platform configuration directory while migrating the previous settings file.

## [0.1.1] - 2026-07-13

- Add project and release links to the About dialog.
- Add a manual update check against the latest GitHub release.
- Show friendlier labels for known DICOM metadata values while keeping raw UIDs and codes visible.
- Make metadata table values selectable for easier copying.

## [0.1.0] - 2026-07-06

- Initial open-source release of Dicron.
- Add local DICOM file and folder loading with Patient / Study / Series browsing.
- Add image viewing, slice navigation, playback, window/level controls, and metadata search.
- Add Linux, macOS, and Windows packaging support.
