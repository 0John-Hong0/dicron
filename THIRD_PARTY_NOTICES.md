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

## Rust Dependencies

Rust dependencies are resolved through Cargo and recorded in `Cargo.lock`.
Review each crate's published license metadata before redistributing modified binaries in a different packaging context.
