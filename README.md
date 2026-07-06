# Dicron

A small native DICOM image viewer built with Rust and egui.

`dicron` is focused on quickly opening local DICOM files and folders, browsing studies as a patient / study / series tree, inspecting tags, and adjusting window level without pulling in a large PACS workstation stack.

> This project is not a diagnostic medical device. Do not use it for clinical decisions.

## Features

- Open individual DICOM files or scan folders recursively.
- Drag and drop DICOM files or folders onto the viewer.
- Browse indexed studies in a Patient / Study / Series / Slice tree.
- View single-frame and multi-frame DICOM images.
- Step through slices with arrow keys, the viewer scroll wheel, or side scrollbar.
- Autoplay image stacks with adjustable FPS and loop mode.
- Adjust window center and window width interactively.
- Inspect curated DICOM tags or search across all loaded tags.
- Native desktop builds for Linux, macOS, and Windows.
- Bundled Geist UI fonts, with Source Han Sans CJK fallback support.

## Status

This is a local inspection tool for DICOM images, metadata, and series structure. It is intended for development, exploration, and troubleshooting workflows.

The Linux build currently forces the X11 windowing backend because file drag and drop is handled reliably there by the current `winit` stack.

## Getting Started

Download packaged builds from [GitHub Releases](https://github.com/0John-Hong0/dicron/releases), or build locally from source.

Install Rust, then clone and build the project:

```sh
./scripts/fetch-fonts.sh
cargo build --release
```

Run the viewer:

```sh
cargo run --release
```

You can also pass files or folders on the command line:

```sh
cargo run --release -- /path/to/dicom-or-folder
```

## Fonts

The repository includes Geist font files. Source Han Sans is intentionally fetched by script because the font file is large:

```sh
./scripts/fetch-fonts.sh
```

Run this before release/package builds if `assets/fonts/SourceHanSans.ttc` is not present.

Font licenses are stored in `assets/licenses/`; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## Development

Useful local checks:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution expectations and pull request guidance.

## Packaging

Release builds are driven by `.github/workflows/release.yml` from `main`.

The current workflow builds:

- Debian package with `cargo deb`
- Arch package
- macOS app bundle packaged as Apple Silicon and Intel DMGs
- Windows executable and NSIS installer

For Debian packaging:

```sh
./scripts/fetch-fonts.sh
cargo install cargo-deb --locked
cargo deb
```

For Windows cross-build helpers, see:

```sh
./scripts/build-windows.sh
./scripts/package-windows-installer.sh
```

For macOS packaging, build on macOS and run:

```sh
./scripts/fetch-fonts.sh
cargo build --release --locked
./scripts/package-macos.sh
```

macOS DMGs are currently unsigned and not notarized.

Release maintainers should follow [docs/RELEASING.md](docs/RELEASING.md).

## Project Layout

```text
src/
  app/                egui app state, layout, viewer controls, tree, and loading flow
  dicom/              Folder indexing, pixel loading, and DICOM value helpers
  metadata.rs         DICOM metadata extraction
  metadata_table.rs   Tag table UI
assets/
  fonts/              Bundled/fetched fonts
  licenses/           Font license texts
packaging/
  linux/              Desktop entry and Linux package assets
  macos/              macOS app bundle metadata
  windows/            NSIS installer definition
scripts/              Build and packaging helpers
```

## Community

- Bugs and feature requests: [GitHub Issues](https://github.com/0John-Hong0/dicron/issues)
- Pull requests: see [CONTRIBUTING.md](CONTRIBUTING.md)
- Conduct expectations: see [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)
- Security reports: see [SECURITY.md](SECURITY.md)
- Support notes: see [SUPPORT.md](SUPPORT.md)

## Copyright and License

Copyright (C) 2026 0John-Hong0

Dicron is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

See [LICENSE](LICENSE) for details.

Bundled fonts have their own licenses:

- Geist: `assets/licenses/LICENSE-Geist.txt`
- Source Han Sans: `assets/licenses/LICENSE-SourceHanSans.txt`

See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for more detail.
