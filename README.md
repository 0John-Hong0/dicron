# Dicron

Dicron is a native desktop viewer for quickly opening local DICOM files and folders, browsing studies, inspecting metadata, and adjusting window/level.

> Dicron is not a diagnostic medical device. Do not use it for clinical decisions.

## Features

- Open individual DICOM files or scan folders recursively.
- Drag and drop DICOM files or folders onto the viewer.
- Browse indexed studies as compact series cards with fixed middle-slice thumbnails, or switch to
  the full Patient / Study / Series / Slice filename tree.
- View single-frame and multi-frame DICOM images.
- Step through slices with the keyboard, mouse wheel, or viewer scrollbar.
- Autoplay image stacks with adjustable FPS and loop mode.
- Adjust window center and width interactively or apply common window presets.
- Reconstruct a series into interactive three-plane MPR, thick-slab MIP, and GPU volume rendering views.
- Inspect curated DICOM tags or search across all loaded tags.
- Native desktop builds for Linux and Windows.

## Download

Download the latest version from [GitHub Releases](https://github.com/0John-Hong0/dicron/releases):

- Windows: installer executable
- Debian and Ubuntu: `.deb` package
- Arch Linux: `.pkg.tar.zst` package

On Linux, Dicron currently uses X11/XWayland.

## Usage

Open Dicron and choose **Open DICOM** or **Open Folder**, or drag files and folders directly onto the window.

In the **DICOM Tree**, use **Series previews** for one fixed thumbnail, description, modality, and
slice count per series. Choose **File list** when you need the individual filenames.

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
- **Right-drag vertically over the image**: drag down to zoom in, up to zoom out around the drag-start pointer.
- **Middle-drag over the image**: pan the image.
- **Double-click the image**: fit and center the image in the viewer.
- **Flip H**, **Flip V**, and **Rotate 90°**: change the image orientation while preserving its metadata overlays.
- **Reset Display**: restore zoom, pan, orientation, and window level.
- **0–9**: apply the corresponding entry in the **Preset** menu. `0` restores
  the DICOM window and `1` fits the current image's full value range.

Use **Edit WL** to enter an exact window level and width.

3D views:

Select a grayscale series, choose **3D** at the left of the viewer controls, and pick **MPR**, **MIP**, or
**Volume Rendering**. Dicron reconstructs the series into a volume and replaces the viewer controls with the
3D controls; **Cancel** in the status bar, **Escape**, or selecting another series stops the preparation.
The planes open on the slice the 2D viewer shows. **2D** or **Escape** returns to the 2D viewer, and the
reconstructed volume stays ready for the next visit. Selecting another series while a 3D view is open
reconstructs that series in the same view, and returning to a series restores its 3D view as you left it.
Each view names the patient, study, and series it shows.

**MPR** and **MIP** show the volume as three panels, axial, coronal, and sagittal:

- **Drag over a panel**: move the crosshair, which sets the slice the other two panels show.
- **Right-drag vertically over a panel**: zoom that panel around the drag start.
- **Middle-drag over a panel**: pan that panel.
- **Mouse wheel over a panel**: step through that panel's own slices; scrolling down moves towards the feet,
  the back, or the patient's left.
- **Arrow keys**, **Page Up / Page Down**, **Home / End**: step the axial plane along the head-to-foot axis:
  up towards the head, down towards the feet.
- **WL**, **WW**, and **Reset WL**: window the planes.
- **Quality**: choose the target voxel size the planes are sampled at. A size finer than the series or
  too large for memory is coarsened, and the menu shows the size used. Rounded voxel counts
  preserve the volume's physical size, including short stacks.
- **Reset View**: re-center the crosshair and restore the fitted zoom, pan, and slab thickness. Changing
  **Quality** keeps the crosshair, slabs, and zoom where they are.
- In **MIP** each panel projects the brightest voxel inside a slab of whole slices. The dashed lines on
  either side of a crosshair line enclose exactly the slices of the plane that line marks. **Drag a
  dashed boundary** to change the slab thickness of that plane, not of the panel you drag in; the
  control row shows the thickness each slab covers.

**Volume Rendering** ray-casts the whole volume on the GPU:

- **Left-drag**: rotate the volume.
- **Right-drag vertically** or **mouse wheel**: zoom.
- **Middle-drag**: pan.
- **Double-click** or **Reset View**: restore the default view, which looks at the patient from the
  front with the head up. The letters at the edges name the patient direction each one faces.
- **Preset**: choose the transfer function that maps values to color and opacity. CT series open on
  **CT Soft Tissue** and other modalities on **Grayscale**, which follows **WL** and **WW**.
- **Shading**: light surfaces by the value gradient.

3D reconstruction needs a series of at least two single-frame grayscale images. Every image must
record its position and orientation in the patient and its pixel spacing, and all images must share
the same matrix size, orientation, and pixel spacing. The slice positions must describe one
stack without duplicates or sideways shifts. Unevenly spaced slices, such as a stack missing a
slice, are placed at their recorded positions and the gaps between them are interpolated. Other
series are rejected with an explanation. Selecting such a series while a 3D view is open returns to it in the 2D viewer.

Series whose slices are 3 mm or thicker, or 3 mm or more apart, are reported as a low-resolution
source before the 3D views open,
because their coronal, sagittal, and volume views may show stair-stepping or blurring. Choose
**Open Low-resolution 3D** to continue anyway.
Cancelling the warning returns to the selected series in 2D.

## Community

- Bugs and feature requests: [GitHub Issues](https://github.com/0John-Hong0/dicron/issues)
- Pull requests: see [CONTRIBUTING.md](CONTRIBUTING.md)
- Conduct expectations: see [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)
- Security reports: see [SECURITY.md](SECURITY.md)
- Support notes: see [SUPPORT.md](SUPPORT.md)

## License

Dicron is licensed under the [GNU General Public License v3.0 or later](LICENSE).

Bundled fonts have separate licenses. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for details.
