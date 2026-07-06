# Releasing

Releases are built from `main` by `.github/workflows/release.yml`.

## Checklist

1. Make sure `main` is clean and up to date.
2. Update the version in `Cargo.toml`.
3. Update the root `dicron` package version in `Cargo.lock`.
4. Update `CHANGELOG.md`.
5. Run:

   ```sh
   cargo fmt --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test
   cargo build --release
   ```

6. Commit and push the version bump.
7. Run the GitHub Actions `Release` workflow on `main`.

The workflow creates the `vX.Y.Z` tag and GitHub release, then uploads Debian, Arch, macOS Apple Silicon, macOS Intel, and Windows artifacts.

Use the workflow's `replace_existing` option only when intentionally replacing a failed or incomplete release for the same version.
