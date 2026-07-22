//! GitHub release lookup and semantic-version comparison.

use std::time::Duration;

use anyhow::{Context, Result};
use semver::Version;
use serde::Deserialize;

const GITHUB_LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/0John-Hong0/dicron/releases/latest";
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub(crate) enum UpdateCheckOutcome {
    UpToDate {
        latest_tag: String,
    },
    UpdateAvailable {
        latest_tag: String,
        release_url: String,
    },
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
}

pub(crate) fn check_latest_release(current_version: &str) -> Result<UpdateCheckOutcome> {
    let release = fetch_latest_release()?;
    compare_release_versions(current_version, &release.tag_name, release.html_url)
}

fn fetch_latest_release() -> Result<GitHubRelease> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(UPDATE_CHECK_TIMEOUT))
        .build()
        .into();

    let release = agent
        .get(GITHUB_LATEST_RELEASE_API_URL)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", concat!("dicron/", env!("CARGO_PKG_VERSION")))
        .call()
        .context("failed to request the latest GitHub release")?
        .body_mut()
        .read_json()
        .context("failed to parse the latest GitHub release")?;

    Ok(release)
}

fn compare_release_versions(
    current_version: &str,
    latest_tag: &str,
    release_url: String,
) -> Result<UpdateCheckOutcome> {
    let current = Version::parse(normalize_release_version(current_version))
        .with_context(|| format!("failed to parse current version {current_version:?}"))?;
    let latest = Version::parse(normalize_release_version(latest_tag))
        .with_context(|| format!("failed to parse latest release tag {latest_tag:?}"))?;

    let outcome = if latest > current {
        UpdateCheckOutcome::UpdateAvailable {
            latest_tag: latest_tag.to_owned(),
            release_url,
        }
    } else {
        UpdateCheckOutcome::UpToDate {
            latest_tag: latest_tag.to_owned(),
        }
    };

    Ok(outcome)
}

fn normalize_release_version(version: &str) -> &str {
    let version = version.trim();

    version
        .strip_prefix('v')
        .or_else(|| version.strip_prefix('V'))
        .unwrap_or(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_available_update_from_prefixed_tag() {
        let outcome =
            compare_release_versions("0.1.0", "v0.2.0", "https://example.com".to_owned()).unwrap();

        assert!(matches!(
            outcome,
            UpdateCheckOutcome::UpdateAvailable { latest_tag, .. } if latest_tag == "v0.2.0"
        ));
    }

    #[test]
    fn detects_up_to_date_release() {
        let outcome =
            compare_release_versions("0.1.0", "v0.1.0", "https://example.com".to_owned()).unwrap();

        assert!(matches!(
            outcome,
            UpdateCheckOutcome::UpToDate { latest_tag } if latest_tag == "v0.1.0"
        ));
    }

    #[test]
    fn accepts_unprefixed_release_tag() {
        let outcome =
            compare_release_versions("0.1.0", "0.1.1", "https://example.com".to_owned()).unwrap();

        assert!(matches!(
            outcome,
            UpdateCheckOutcome::UpdateAvailable { latest_tag, .. } if latest_tag == "0.1.1"
        ));
    }
}
