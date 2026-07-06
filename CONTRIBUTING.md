# Contributing

Thanks for helping improve Dicron. This is a small native Rust/egui app, so contributions are easiest to review when they stay focused and include a short explanation of the behavior they change.

## Ground Rules

- Keep the app non-diagnostic. Do not describe or market it as medical decision software.
- Avoid committing patient data, private DICOM studies, screenshots containing PHI, or proprietary test files.
- Do not report security vulnerabilities in public issues; follow [SECURITY.md](SECURITY.md).
- Keep changes scoped. Separate behavior, refactors, packaging, and documentation into different pull requests when practical.
- Follow the existing Rust style and use small helper functions when UI code gets deeply nested.

## Local Setup

Install stable Rust, then fetch the large CJK fallback font when building locally:

```sh
./scripts/fetch-fonts.sh
cargo build
```

Common development commands:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

## Pull Requests

Before opening a PR:

1. Run `cargo fmt --check`.
2. Run `cargo clippy --all-targets --all-features -- -D warnings`.
3. Run `cargo test`.
4. Update `README.md`, `CHANGELOG.md`, or packaging docs when user-visible behavior changes.

Good PR descriptions include:

- what changed
- why it changed
- how it was tested
- screenshots or short screen recordings for UI changes

## Release Changes

Version bumps are done in `Cargo.toml` and `Cargo.lock`. Releases are built by the GitHub Actions release workflow from `main`; see [docs/RELEASING.md](docs/RELEASING.md).
