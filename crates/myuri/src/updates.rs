//! Check the project's published releases on GitHub.

use semver::Version;

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

pub enum ReleaseStatus {
    Current,
    Unpublished,
    Available { version: Version, page: String },
}

/// Blocks on GitHub. Run it on a worker.
pub fn check() -> Result<ReleaseStatus, String> {
    let repo = REPOSITORY
        .strip_prefix("https://github.com/")
        .ok_or_else(|| "无法确定 GitHub 仓库".to_string())?;
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let headers = [(
        "accept".to_string(),
        "application/vnd.github+json".to_string(),
    )];
    let (status, body) = crate::net::get_status(&url, &headers)?;
    match status {
        200 => release_status(
            &body,
            &Version::parse(crate::VERSION).expect("valid Cargo version"),
        ),
        404 => Ok(ReleaseStatus::Unpublished),
        _ => Err(format!("GitHub 返回 HTTP {status}")),
    }
}

fn release_status(body: &[u8], installed: &Version) -> Result<ReleaseStatus, String> {
    let invalid = || "GitHub 返回了无法识别的版本号".to_string();
    let release: serde_json::Value = serde_json::from_slice(body).map_err(|_| invalid())?;
    let tag = release["tag_name"].as_str().ok_or_else(invalid)?;
    let version = Version::parse(tag.strip_prefix('v').unwrap_or(tag)).map_err(|_| invalid())?;
    if version.cmp_precedence(installed).is_gt() {
        let page = format!("{REPOSITORY}/releases/tag/{tag}");
        Ok(ReleaseStatus::Available { version, page })
    } else {
        Ok(ReleaseStatus::Current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_tags_are_compared_as_versions() {
        let installed = Version::parse("1.10.0").unwrap();
        for tag in ["v1.9.99", "v1.10.0", "v1.10.0+build.1"] {
            let body = serde_json::json!({ "tag_name": tag }).to_string();
            assert!(matches!(
                release_status(body.as_bytes(), &installed),
                Ok(ReleaseStatus::Current)
            ));
        }
        let update = release_status(br#"{"tag_name":"v1.10.1"}"#, &installed).unwrap();
        let ReleaseStatus::Available { version, page } = update else {
            panic!("expected a newer version");
        };
        assert_eq!(version, Version::new(1, 10, 1));
        assert!(page.ends_with("/releases/tag/v1.10.1"));
        assert!(release_status(br#"{"tag_name":"../wrong"}"#, &installed).is_err());
    }
}
