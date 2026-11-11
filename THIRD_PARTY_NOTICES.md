# Third-Party Notices

Dicron is licensed under the GNU General Public License v3.0 or later. See [LICENSE](LICENSE).

The project also includes or downloads third-party assets and dependencies with their own
licenses. All fonts listed below are compiled into the released binary.

The license files named below are in `assets/licenses/` in the source tree. Installed copies of
Dicron keep them in the `licenses` folder of the Windows install folder,
`/usr/share/doc/dicron/licenses/` (Debian package) or `/usr/share/licenses/dicron/` (Arch package).

## Fonts

- Geist Regular and Geist Mono are bundled in `assets/fonts/`.
  Their license is stored at `assets/licenses/LICENSE-Geist.txt`.
- Source Han Sans Medium is downloaded by `scripts/fetch-fonts.sh` and compiled into
  the binary for CJK fallback support. It is Adobe's official standalone face from the
  2.005R tag, used unmodified; the full 45-face collection is not distributed.
  Its license is stored at `assets/licenses/LICENSE-SourceHanSans.txt`.
- egui's built-in fonts, from the `epaint_default_fonts` crate, are the fallback for symbols
  and scripts the fonts above do not cover:
  - Ubuntu Light, under the Ubuntu Font Licence 1.0, stored at
    `assets/licenses/LICENSE-Ubuntu-Font.txt`.
  - Hack Regular, under the MIT License and the Bitstream Vera License, stored at
    `assets/licenses/LICENSE-Hack.txt`.
  - Noto Emoji Regular, under the SIL Open Font License 1.1, stored at
    `assets/licenses/LICENSE-NotoEmoji.txt`.
  - emoji-icon-font, under the MIT License, stored at
    `assets/licenses/LICENSE-emoji-icon-font.txt`.

## Native Libraries

These C and C++ libraries decode compressed DICOM images. Their Rust `-sys` crates build
bundled copies, and the Arch package links the system CharLS library.

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

## Microsoft Visual C++ Runtime

The Windows installer places the Microsoft Visual C++ runtime DLLs (such as `vcruntime140.dll`
and `msvcp140.dll`) next to `dicron.exe`, so Dicron starts without a separate runtime install.
They are Microsoft redistributable files, distributed under the Visual Studio license terms.
See [Microsoft's redistribution documentation](https://learn.microsoft.com/en-us/cpp/windows/redistributing-visual-cpp-files).

## Rust Dependencies

Rust dependencies are resolved through Cargo, recorded in `Cargo.lock`, and compiled into the
released binary. `THIRD_PARTY_LICENSES.html` lists every one of these crates with its license
text. Each release attaches it as an asset and installs it with Dicron. To generate it from
the source tree, install [cargo-about](https://github.com/EmbarkStudios/cargo-about) and run:

```sh
./scripts/generate-third-party-licenses.sh
```

The report is written to `dist/THIRD_PARTY_LICENSES.html`.
