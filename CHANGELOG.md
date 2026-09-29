# Changelog

All notable user-facing changes should be recorded here.

This project uses semantic versioning while it remains practical for a small desktop app.

## [Unreleased]

- Keep the window responsive while images open, and show the loading progress bar right away in
  one steady place from folder scanning until the image appears.
- Add a compact Encoding menu with Auto, DICOM default, UTF-8, Korean (EUC-KR), and Japanese
  (Shift-JIS) choices. Auto uses strict detection only when no character set is declared;
  changing modes reopens the current source while preserving the selected image and view.
- Fix opening DICOM images with an empty optional VOI LUT Function value.

## [0.3.0] - 2026-09-09

- Split series preview details into separate modality/description, series number, and slice-count
  lines for easier scanning.
- Add a compact DICOM tree mode with one fixed middle-slice thumbnail per series, including the
  series description, modality, and slice count. Series reopen at their last viewed slice, while
  the original filename list remains available as a persisted view option.
- Improve series-preview thumbnail loading with lower memory use, bounded background decoding,
  stale-work cancellation, and first-slice fallback when the middle slice cannot be decoded.
- Remove the second scrollbar that appeared inside the DICOM tree for very long series. Long
  series now scroll with the rest of the tree while still laying out only the visible rows.
- Pin the Patient, Study, and Series headers of the rows in view to the top of the DICOM tree
  while scrolling. Clicking a pinned header collapses that node.
- Speed up initial DICOM scans by skipping unused header decoding and using larger buffered reads
  to reduce read overhead.

## [0.2.1] - 2026-09-03

- Fix mouse wheel slice navigation so one wheel notch advances exactly one slice without delayed,
  swallowed, or duplicate steps.
- Reduce the installed size by embedding only the single Source Han Sans face the app uses instead
  of the full 45-face collection. CJK coverage is unchanged.

## [0.2.0] - 2026-08-22

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
- Speed up large file and folder opens with compact parallel header indexing, constant-time hierarchy grouping, and coalesced progress updates.
- Publish automated Debian, Arch Linux, and Windows releases only after every package build succeeds; unsigned macOS packages are no longer produced.
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
