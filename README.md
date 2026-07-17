# Dicron

Dicron is a native desktop viewer for quickly opening local DICOM files and folders, browsing studies, inspecting metadata, and adjusting window/level.

> Dicron is not a diagnostic medical device. Do not use it for clinical decisions.

## Features

- Open individual DICOM files or scan folders recursively.
- Drag and drop DICOM files or folders onto the viewer.
- Browse indexed studies in a Patient / Study / Series / Slice tree.
- View single-frame and multi-frame DICOM images.
- Step through slices with the keyboard, mouse wheel, or viewer scrollbar.
- Autoplay image stacks with adjustable FPS and loop mode.
- Adjust window center and width interactively.
- Inspect curated DICOM tags or search across all loaded tags.
- Native desktop builds for Linux, macOS, and Windows.

## Download

Download the latest version from [GitHub Releases](https://github.com/0John-Hong0/dicron/releases):

- Windows: installer executable
- macOS: DMG for Apple Silicon or Intel
- Debian and Ubuntu: `.deb` package
- Arch Linux: `.pkg.tar.zst` package

macOS builds are currently unsigned and not notarized. On Linux, Dicron currently uses X11/XWayland.

## Usage

Open Dicron and choose **Open DICOM** or **Open Folder**, or drag files and folders directly onto the window.

You can also open one or more paths when launching Dicron from a terminal:

```sh
dicron /path/to/dicom-or-folder
```

Viewer controls:

- **Arrow keys** or **mouse wheel**: move through slices.
- **Page Up / Page Down**: move ten slices at a time.
- **Home / End**: jump to the first or last slice.
- **Drag horizontally over the image**: adjust window width.
- **Drag vertically over the image**: adjust window center.

## Community

- Bugs and feature requests: [GitHub Issues](https://github.com/0John-Hong0/dicron/issues)
- Pull requests: see [CONTRIBUTING.md](CONTRIBUTING.md)
- Conduct expectations: see [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)
- Security reports: see [SECURITY.md](SECURITY.md)
- Support notes: see [SUPPORT.md](SUPPORT.md)

## License

Dicron is licensed under the [GNU General Public License v3.0 or later](LICENSE).

Bundled fonts have separate licenses. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for details.
