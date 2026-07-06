# Third-Party Notices

Dicron is licensed under the GNU General Public License v3.0 or later. See [LICENSE](LICENSE).

The project also includes or fetches third-party assets and dependencies with their own licenses.

## Fonts

- Geist Regular and Geist Mono are bundled in `assets/fonts/`.
  Their license is stored at `assets/licenses/LICENSE-Geist.txt`.
- Source Han Sans is fetched by `scripts/fetch-fonts.sh` for CJK fallback support.
  Its license is stored at `assets/licenses/LICENSE-SourceHanSans.txt`.

## Rust Dependencies

Rust dependencies are resolved through Cargo and recorded in `Cargo.lock`.
Review each crate's published license metadata before redistributing modified binaries in a different packaging context.
