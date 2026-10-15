//! Thin wrappers over the `herdr` CLI. Remote calls go through
//! `herdr --machine <profile-id>`, never the label: two profiles with the same
//! label make the label ambiguous.

use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::{ssh, ui, Coded};

pub fn bin() -> String {
    std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into())
}

pub struct Profile {
    pub id: String,
    pub label: String,
    pub target: String,
}

impl Profile {
    /// Matched on both target and label so a user's own profile is never touched.
    pub fn is_for(&self, name: &str) -> bool {
        self.label == name && self.target == ssh::alias(name)
    }
}

pub fn profile_id(name: &str) -> Result<Option<String>> {
    Ok(profiles()?
        .into_iter()
        .find(|p| p.is_for(name))
        .map(|p| p.id))
}

/// False when the herdr CLI is not installed; headless agents work without it.
pub fn available() -> bool {
    Command::new(bin())
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub fn missing() -> anyhow::Error {
    Coded::new(
        "herdr_missing",
        "Herdr is not installed here; pane agents and `attach` need it. Headless agents (--headless) work without it",
    )
    .into()
}

pub fn profiles() -> Result<Vec<Profile>> {
    let output = match Command::new(bin())
        .args(["machine", "list", "--json"])
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).context("run `herdr machine list`"),
    };
    if !output.status.success() {
        bail!(
            "herdr machine list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let list: Vec<Value> =
        serde_json::from_slice(&output.stdout).context("herdr machine list is not JSON")?;
    Ok(list
        .iter()
        .filter_map(|item| {
            let field = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_owned);
            Some(Profile {
                id: field("id")?,
                label: field("label")?,
                target: field("target")?,
            })
        })
        .collect())
}

/// Adds the Herdr profile unless one already exists. Re-adding would create a
/// duplicate profile.
pub fn ensure_profile(name: &str) -> Result<String> {
    if let Some(id) = profile_id(name)? {
        return Ok(id);
    }
    let mut cmd = Command::new(bin());
    cmd.args([
        "machine",
        "add",
        &ssh::alias(name),
        "--label",
        name,
        "--remote-session",
        "default",
    ]);
    // Keep stdout clean for --json callers; herdr's progress goes to stderr.
    cmd.stdout(std::io::stderr());
    if !ui::interactive() {
        cmd.stdin(Stdio::null());
    }
    let status = cmd.status().context("run `herdr machine add`")?;
    if !status.success() {
        bail!(Coded::new(
            "herdr_failed",
            format!("herdr machine add {} exited {status}", ssh::alias(name)),
        ));
    }
    profile_id(name)?.context("herdr machine add did not save a profile")
}

pub fn remove_profile(name: &str) -> Result<bool> {
    let Some(id) = profile_id(name)? else {
        return Ok(false);
    };
    let output = Command::new(bin())
        .args(["machine", "remove", &id])
        .stdin(Stdio::null())
        .output()
        .context("run `herdr machine remove`")?;
    if !output.status.success() {
        bail!(
            "herdr machine remove {id} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(true)
}

/// Waits for the remote Herdr server behind `profile` to answer.
pub async fn wait_reachable(profile: &str, limit: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let result = remote(profile, &["workspace", "list"]).await;
        match result {
            Ok(_) => return Ok(()),
            Err(err) if tokio::time::Instant::now() >= deadline => {
                return Err(err.context(format!(
                    "the Herdr server on this machine is not answering; try `herdr machine reconnect {profile}`"
                )));
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
}

/// Runs `herdr --machine <profile> <args>` and parses its JSON reply.
pub async fn remote(profile: &str, args: &[&str]) -> Result<Value> {
    let text = remote_text(profile, args).await?;
    serde_json::from_str(&text)
        .with_context(|| format!("herdr {} returned non-JSON output", args.join(" ")))
}

/// Same as `remote`, for commands that print plain text (`agent read`).
pub async fn remote_text(profile: &str, args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new(bin())
        .arg("--machine")
        .arg(profile)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("run herdr")?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    // herdr reports errors as {"error":{"code","message"}} on stdout or stderr.
    let parse = |bytes: &[u8]| -> Option<(String, String)> {
        let value: Value = serde_json::from_slice(bytes).ok()?;
        let error = value.get("error")?;
        Some((
            error.get("code")?.as_str()?.to_owned(),
            error.get("message")?.as_str()?.to_owned(),
        ))
    };
    if let Some((code, message)) = parse(&output.stderr).or_else(|| parse(&output.stdout)) {
        bail!(Coded::owned(code, message));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(Coded::new(
        "herdr_failed",
        format!(
            "herdr {} exited {}: {}",
            args.first().copied().unwrap_or_default(),
            output.status,
            stderr.trim()
        ),
    ));
}
