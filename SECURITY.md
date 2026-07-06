# Security Policy

## Supported Versions

Security fixes target the latest release and the current `main` branch.

## Reporting a Vulnerability

Please do not open a public issue for vulnerabilities involving crashes, unsafe file handling, path traversal, bundled binaries, release artifacts, or dependency supply-chain problems.

Report privately through GitHub's private vulnerability reporting if it is enabled for the repository.

If private vulnerability reporting is not available, do not post technical details publicly. Open a minimal public issue asking for a private security contact channel, or use contact information listed on the maintainer's GitHub profile.

Include:

- affected version or commit
- operating system
- steps to reproduce
- whether the issue requires a malicious DICOM file, crafted path, or local user interaction
- any logs or stack traces that do not include private patient data

## Medical Data

Do not send real patient DICOM files, screenshots with PHI, or private study metadata. Reproduce issues with synthetic or de-identified data whenever possible.
