//! Keeps niks3 pins on the latest successful outputs of pinned branches.
//!
//! Each pin covers one output of one attribute, so a failing attribute keeps
//! its last good output pinned while the others move on. Pins are named
//! deterministically and recorded in the `pins` table, which is how Amphi
//! knows which niks3 pins it may own. Only the finalizer changes pins, and
//! it clears a pin's recorded path before asking niks3 for it, so that a
//! response that never arrives leaves the pin to be set again.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, bail};
use reqwest::StatusCode;
use sha2::{Digest, Sha256};

use super::Server;
use super::config::{Niks3Config, read_secret};
use super::model::{AttrResult, AttrRow, Run};
use super::reconciler::CONFIG_JOBSET;

/// niks3 limits pin names to 256 characters.
const MAX_NAME: usize = 256;

pub struct Niks3 {
    http: reqwest::Client,
    server_url: String,
    token: String,
}

enum PinOutcome {
    Pinned,
    /// niks3 does not track the path, e.g. because it came from another cache.
    NotInCache,
}

impl Niks3 {
    pub fn new(config: &Niks3Config, http: reqwest::Client) -> anyhow::Result<Self> {
        Ok(Self {
            http,
            server_url: config.server_url.trim_end_matches('/').to_owned(),
            token: read_secret(&config.token_file)?,
        })
    }

    fn url(&self, name: &str) -> String {
        format!("{}/api/pins/{name}", self.server_url)
    }

    async fn put(&self, name: &str, store_path: &str) -> anyhow::Result<PinOutcome> {
        let response = self
            .http
            .post(self.url(name))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "store_path": store_path }))
            .send()
            .await
            .context("creating pin")?;
        match response.status() {
            status if status.is_success() => Ok(PinOutcome::Pinned),
            StatusCode::NOT_FOUND => Ok(PinOutcome::NotInCache),
            status => bail!(
                "creating pin {name}: {status}: {}",
                response.text().await.unwrap_or_default()
            ),
        }
    }

    async fn delete(&self, name: &str) -> anyhow::Result<()> {
        let response = self
            .http
            .delete(self.url(name))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("deleting pin")?;
        match response.status() {
            status if status.is_success() || status == StatusCode::NOT_FOUND => Ok(()),
            status => bail!(
                "deleting pin {name}: {status}: {}",
                response.text().await.unwrap_or_default()
            ),
        }
    }
}

/// A readable pin name, made unique by a hash of its parts.
pub fn pin_name(repo: &str, branch: &str, jobset: &str, attr: &str, output: &str) -> String {
    let key = [repo, branch, jobset, attr, output].join("\0");
    let hash = hex::encode(&Sha256::digest(key.as_bytes())[..4]);
    let readable: String = format!("amphi.{repo}.{branch}.{jobset}.{attr}.{output}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(MAX_NAME - hash.len() - 1)
        .collect();
    format!("{readable}.{hash}")
}

struct PinRow {
    name: String,
    attr: String,
    store_path: Option<String>,
}

async fn jobset_pins(server: &Server, run: &Run, branch: &str) -> sqlx::Result<Vec<PinRow>> {
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT name, attr, store_path FROM pins WHERE repo = $1 AND branch = $2 AND jobset = $3",
    )
    .bind(&run.repo)
    .bind(branch)
    .bind(&run.jobset)
    .fetch_all(&server.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, attr, store_path)| PinRow {
            name,
            attr,
            store_path,
        })
        .collect())
}

async fn release(server: &Server, niks3: &Niks3, name: &str) -> anyhow::Result<()> {
    niks3.delete(name).await?;
    forget(server, name).await?;
    tracing::info!(pin = name, "released pin");
    Ok(())
}

async fn forget(server: &Server, name: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM pins WHERE name = $1")
        .bind(name)
        .execute(&server.db)
        .await?;
    Ok(())
}

/// Moves the pins of the branch's jobset to the run's results.
pub async fn update(server: &Server, run: &Run, branch: &str) -> anyhow::Result<()> {
    let Some(niks3) = &server.niks3 else {
        return Ok(());
    };
    let attrs = AttrRow::of_run(&server.db, run.id).await?;
    let existing = jobset_pins(server, run, branch).await?;

    let mut wanted = BTreeMap::new();
    let mut succeeded = HashSet::new();
    for attr in &attrs {
        if !matches!(
            attr.result,
            Some(AttrResult::Succeeded | AttrResult::Cached)
        ) {
            continue;
        }
        succeeded.insert(attr.attr.as_str());
        for (output, path) in attr.outputs.iter().flat_map(|outputs| outputs.0.iter()) {
            if let Some(path) = path {
                let name = pin_name(&run.repo, branch, &run.jobset, &attr.attr, output);
                wanted.insert(name, (attr.attr.as_str(), output.as_str(), path.as_str()));
            }
        }
    }

    for (name, (attr, output, path)) in &wanted {
        let pin = existing.iter().find(|pin| pin.name == *name);
        if pin.is_some_and(|pin| pin.store_path.as_deref() == Some(*path)) {
            continue;
        }
        sqlx::query(
            "INSERT INTO pins (name, repo, branch, jobset, attr, output, run_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (name) DO UPDATE SET store_path = NULL",
        )
        .bind(name)
        .bind(&run.repo)
        .bind(branch)
        .bind(&run.jobset)
        .bind(attr)
        .bind(output)
        .bind(run.id)
        .execute(&server.db)
        .await?;
        match niks3.put(name, path).await? {
            PinOutcome::Pinned => {
                sqlx::query(
                    "UPDATE pins SET store_path = $2, run_id = $3, updated_at = now() \
                     WHERE name = $1",
                )
                .bind(name)
                .bind(path)
                .bind(run.id)
                .execute(&server.db)
                .await?;
            }
            // The old output is no longer this attribute's result either.
            PinOutcome::NotInCache if pin.is_some() => release(server, niks3, name).await?,
            // niks3 made no pin of that name.
            PinOutcome::NotInCache => forget(server, name).await?,
        }
    }

    // Attributes that are gone, and outputs that succeeding attributes no
    // longer have.
    let evaluated: HashSet<&str> = attrs.iter().map(|attr| attr.attr.as_str()).collect();
    for pin in &existing {
        if !evaluated.contains(pin.attr.as_str())
            || (succeeded.contains(pin.attr.as_str()) && !wanted.contains_key(&pin.name))
        {
            release(server, niks3, &pin.name).await?;
        }
    }
    Ok(())
}

/// Releases the pins that no branch wants: those of branches that are no
/// longer pinned or no longer exist, and of jobsets the branch's head has no
/// run of, e.g. after a rename. A head whose `amphi.toml` is invalid says
/// nothing about its jobsets, so it keeps them all.
pub async fn release_unwanted(server: &Server) -> anyhow::Result<()> {
    let Some(niks3) = &server.niks3 else {
        return Ok(());
    };
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT p.name FROM pins p WHERE NOT EXISTS ( \
           SELECT 1 FROM refs f JOIN runs r ON r.repo = f.repo AND r.sha = f.sha \
           WHERE f.repo = p.repo AND f.kind = 'branch' AND f.name = p.branch AND f.pin \
           AND r.jobset IN (p.jobset, $1))",
    )
    .bind(CONFIG_JOBSET)
    .fetch_all(&server.db)
    .await?;
    for name in names {
        release(server, niks3, &name).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_names_are_valid_and_distinct() {
        let name = pin_name(
            "merrkry/infra",
            "release/1.0",
            "x86_64-linux",
            "nur.foo+bar",
            "out",
        );
        assert!(name.starts_with("amphi.merrkry-infra.release-1.0.x86_64-linux.nur.foo-bar.out."));
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        );
        assert_ne!(
            pin_name("o/r", "a/b", "j", "x", "out"),
            pin_name("o/r", "a-b", "j", "x", "out"),
        );
        let long = pin_name("o/r", &"b".repeat(300), "j", "x", "out");
        assert_eq!(long.len(), MAX_NAME);
    }
}
