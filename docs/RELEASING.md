# Releasing

Releases are built from `main` by `.github/workflows/release.yml`.

Repository Actions settings must allow GitHub Actions to create pull requests. New-release mode uses the workflow's `GITHUB_TOKEN` to open the release preparation pull request.

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
5. Run the GitHub Actions `Release` workflow on `main` in `new` mode and choose the semantic version bump (`patch`, `minor`, or `major`).
6. Review the generated `release/vX.Y.Z` pull request, complete its checks, and merge it into `main`. The same workflow run waits for the merge and continues automatically.

New-release mode bumps the package version in `Cargo.toml` and `Cargo.lock`, promotes the `Unreleased` changelog entries to that version, opens a release preparation pull request, and waits for it to merge. It then tags the merged release commit, creates a draft GitHub release using that changelog section as the release notes, uploads Debian, Arch, and Windows artifacts, and publishes the release only after every package build succeeds.

Use `retry` mode after the tag exists to resume an interrupted release. It resumes an existing draft release or recreates a missing draft from that tag; it cannot replace a published release.
