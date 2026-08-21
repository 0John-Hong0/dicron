# Releasing

Releases are built from `main` by `.github/workflows/release.yml`.

## Checklist

1. Make sure `main` is clean and up to date.
2. Review the current package version and make sure `CHANGELOG.md` has entries under `Unreleased`.
3. Run:

   ```sh
   cargo fmt --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test
   cargo build --release
   ```

4. Commit and push all product and changelog changes.
5. Run the GitHub Actions `Release` workflow on `main`:
   - Choose `new` and a semantic version bump (`patch`, `minor`, or `major`) for a new release.
   - Choose `retry` only to resume an existing unfinished release for the current package version.

For a new release, the workflow bumps the package version in `Cargo.toml` and `Cargo.lock`, promotes the `Unreleased` changelog entries to that version, commits the release preparation to `main`, creates the `vX.Y.Z` tag and a draft GitHub release, and uses the changelog section as the release notes. It uploads Debian, Arch, and Windows artifacts and publishes the release only after every package build succeeds.

A retry requires the release tag to already exist. It resumes an existing draft release or recreates a missing draft from that tag; it cannot replace a published release.
