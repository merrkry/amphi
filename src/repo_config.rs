//! The `amphi.toml` file a repository uses to tell Amphi what to build.

use anyhow::{Context, bail, ensure};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;

pub const FILE_NAME: &str = "amphi.toml";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct RepoConfig {
    /// Branches built on every push.
    #[serde(default)]
    pub branches: BranchGlobs,
    /// Whether to build the heads of open pull requests from this repository.
    #[serde(default)]
    pub pull_requests: bool,
    /// Branches whose latest successful outputs are pinned in niks3.
    #[serde(default)]
    pub pin: BranchGlobs,
    /// Branches whose failures open a GitHub issue. Elsewhere, the checks
    /// on the commit are enough.
    #[serde(default)]
    pub issues: BranchGlobs,
    pub jobsets: Vec<Jobset>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Jobset {
    pub name: String,
    /// Flake output attribute. Every derivation below it is a job.
    pub attr: String,
}

impl RepoConfig {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let config: Self = toml::from_str(text).context("invalid amphi.toml")?;
        ensure!(!config.jobsets.is_empty(), "amphi.toml defines no jobsets");
        for (index, jobset) in config.jobsets.iter().enumerate() {
            let valid_name = !jobset.name.is_empty()
                && jobset
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
            ensure!(
                valid_name,
                "jobset name {:?} must be non-empty and use only ASCII letters, digits, '.', '_' and '-'",
                jobset.name
            );
            ensure!(
                !jobset.attr.is_empty(),
                "jobset {} has an empty attr",
                jobset.name
            );
            if config.jobsets[..index]
                .iter()
                .any(|other| other.name == jobset.name)
            {
                bail!("jobset {} is defined twice", jobset.name);
            }
        }
        Ok(config)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(try_from = "Vec<String>")]
pub struct BranchGlobs(GlobSet);

impl BranchGlobs {
    pub fn matches(&self, branch: &str) -> bool {
        self.0.is_match(branch)
    }
}

impl TryFrom<Vec<String>> for BranchGlobs {
    type Error = globset::Error;

    fn try_from(patterns: Vec<String>) -> Result<Self, Self::Error> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            // Branch names contain slashes, which `*` should not cross.
            builder.add(GlobBuilder::new(&pattern).literal_separator(true).build()?);
        }
        builder.build().map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let config = RepoConfig::parse(
            r#"
            branches = ["master", "release/*"]
            pull-requests = true
            pin = ["master"]

            [[jobsets]]
            name = "x86_64-linux"
            attr = "buildJobs.x86_64-linux"
            "#,
        )
        .unwrap();
        assert!(config.branches.matches("master"));
        assert!(config.branches.matches("release/1.0"));
        assert!(!config.branches.matches("release/1.0/hotfix"));
        assert!(!config.branches.matches("feature"));
        assert!(config.pull_requests);
        assert!(config.pin.matches("master"));
        assert!(!config.issues.matches("master"));
        assert_eq!(config.jobsets[0].attr, "buildJobs.x86_64-linux");
    }

    #[test]
    fn rejects_invalid_jobsets() {
        let duplicate = r#"
            [[jobsets]]
            name = "a"
            attr = "x"
            [[jobsets]]
            name = "a"
            attr = "y"
        "#;
        assert!(RepoConfig::parse(duplicate).is_err());
        assert!(RepoConfig::parse("jobsets = []").is_err());
        let bad_name = r#"
            [[jobsets]]
            name = "a/b"
            attr = "x"
        "#;
        assert!(RepoConfig::parse(bad_name).is_err());
    }
}
