# Third-Party Notices

Dicron is licensed under the GNU General Public License v3.0 or later. See [LICENSE](LICENSE).

The project also includes or downloads third-party assets and dependencies with their own
licenses. All fonts listed below are compiled into the released binary.

## Fonts

- Geist Regular and Geist Mono are bundled in `assets/fonts/`.
  Their license is stored at `assets/licenses/LICENSE-Geist.txt`.
- Source Han Sans Medium is downloaded by `scripts/fetch-fonts.sh` and compiled into
  the binary for CJK fallback support. It is Adobe's official standalone face from the
  2.005R tag, used unmodified; the full 45-face collection is not distributed.
  Its license is stored at `assets/licenses/LICENSE-SourceHanSans.txt`.

## Native Libraries

These C and C++ libraries are built from source by their Rust `-sys` crates and linked into the
released binary to decode compressed DICOM images.

- libjpeg-turbo 3.1.0, through `turbojpeg-sys`, decodes 12-bit JPEG Extended images.
  It is covered by the IJG License and the Modified (3-clause) BSD License.
  This software is based in part on the work of the Independent JPEG Group.
  Its licenses are stored at `assets/licenses/LICENSE-libjpeg-turbo.txt`.
- OpenJPEG 2.5.3, through `openjpeg-sys`, decodes JPEG 2000 images.
  It is covered by the 2-clause BSD License.
  Its license is stored at `assets/licenses/LICENSE-OpenJPEG.txt`.
- CharLS 2.4.2, through `charls-sys`, decodes JPEG-LS images.
  It is covered by the 3-clause BSD License.
  Its license is stored at `assets/licenses/LICENSE-CharLS.txt`.

## Rust Dependencies

Rust dependencies are resolved through Cargo and recorded in `Cargo.lock`.
Review each crate's published license metadata before redistributing modified binaries in a different packaging context.
