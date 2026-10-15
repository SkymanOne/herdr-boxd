//! One tree of machines: this host's ledger, plus the ledgers of the machines
//! it created, read by running `herdr-boxd --json list` inside them. This host
//! drives the whole walk (the boxd API can exec in any machine on the account),
//! so the nested calls run with `HERDR_BOXD_DEPTH=0` and never recurse.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::machine::{self, Listed};
use crate::{boxd, herdr, ssh, store, ui, Coded};

/// Levels of machines shown below this host.
pub const MAX_DEPTH: u32 = 3;
const DEPTH_ENV: &str = "HERDR_BOXD_DEPTH";
const REMOTE_TIMEOUT: Duration = Duration::from_secs(45);

fn depth() -> u32 {
    std::env::var(DEPTH_ENV)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .map_or(MAX_DEPTH, |depth: u32| depth.min(MAX_DEPTH))
}

/// Runs `herdr-boxd --json ARGS` inside `vm`. None when it is not installed
/// there.
pub async fn remote_json(client: &mut boxd::Client, vm: &str, args: &str) -> Result<Option<Value>> {
    machine::check_machine_name(vm)?;
    let command = format!(
        "p=\"${{HOME:-/home/boxd}}/.local/bin/herdr-boxd\"; [ -x \"$p\" ] || p=$(command -v herdr-boxd) || exit 127; \
         {DEPTH_ENV}=0 BOXD_VM_NAME={vm} exec \"$p\" --json {args}"
    );
    let output = tokio::time::timeout(REMOTE_TIMEOUT, client.exec(vm, &command))
        .await
        .map_err(|_| {
            Coded::new(
                "timeout",
                format!(
                    "herdr-boxd on {vm} did not answer in {}s",
                    REMOTE_TIMEOUT.as_secs()
                ),
            )
        })??;
    if output.code == 127 {
        return Ok(None);
    }
    let output = output.check(&format!("herdr-boxd {args}"))?;
    serde_json::from_slice(&output.stdout)
        .map(Some)
        .with_context(|| format!("herdr-boxd {args} on {vm} printed non-JSON output"))
}

/// This host's machines and, below the running ones, theirs. Sleeping
/// machines are never woken; unreachable ones are marked, not fatal.
pub async fn list(client: &mut boxd::Client) -> Result<Vec<Listed>> {
    let mut roots = machine::list(client).await?;
    expand(client, &mut roots, depth().saturating_sub(1), false).await;
    Ok(roots)
}

/// Fills in `children` for `levels` more levels. With `wake`, sleeping
/// machines are woken so none of their machines are missed.
async fn expand(client: &mut boxd::Client, nodes: &mut [Listed], levels: u32, wake: bool) {
    if levels == 0 {
        return;
    }
    for node in nodes.iter_mut() {
        if node.status == "gone" || (node.status != "running" && !wake) {
            continue;
        }
        if node.status != "running" {
            if let Err(err) = client.ensure_running(&node.name).await {
                node.unreachable = Some(format!("{err:#}"));
                continue;
            }
            node.status = "running".into();
        }
        match remote_json(client, &node.name, "list").await {
            Ok(None) => {}
            Ok(Some(value)) => match serde_json::from_value::<Vec<Listed>>(value) {
                Ok(mut children) => {
                    for child in &mut children {
                        if child.parent.is_empty() {
                            child.parent = node.name.clone();
                        }
                    }
                    node.manages = true;
                    Box::pin(expand(client, &mut children, levels - 1, wake)).await;
                    node.children = children;
                }
                Err(err) => {
                    node.unreachable = Some(format!("herdr-boxd list output: {err}"));
                }
            },
            Err(err) => node.unreachable = Some(format!("{err:#}")),
        }
    }
}

/// Every node, children before their parent: the order to destroy them in.
pub fn deepest_first(nodes: &[Listed]) -> Vec<&Listed> {
    let mut out = Vec::new();
    for node in nodes {
        out.extend(deepest_first(&node.children));
        out.push(node);
    }
    out
}

/// The machines `name` created, and theirs, waking sleeping ones so none are
/// missed. Fails if any of them cannot be checked.
pub async fn descendants(client: &mut boxd::Client, name: &str) -> Result<Vec<Listed>> {
    let Some(mut node) = machine::list(client)
        .await?
        .into_iter()
        .find(|node| node.name == name)
    else {
        return Ok(Vec::new());
    };
    expand(client, std::slice::from_mut(&mut node), MAX_DEPTH, true).await;
    if let Some(stuck) = deepest_first(std::slice::from_ref(&node))
        .into_iter()
        .find(|node| node.unreachable.is_some())
    {
        bail!(Coded::new(
            "unreachable",
            format!(
                "cannot list the machines {} created ({}); nothing was destroyed",
                stuck.name,
                stuck.unreachable.as_deref().unwrap_or_default()
            ),
        ));
    }
    Ok(node.children)
}

/// Agents of every host in the tree that runs herdr-boxd, tagged with `host`.
pub async fn remote_agents(client: &mut boxd::Client) -> Result<Vec<Value>> {
    let tree = list(client).await?;
    let hosts: Vec<String> = deepest_first(&tree)
        .into_iter()
        .filter(|node| node.manages)
        .map(|node| node.name.clone())
        .collect();
    let mut out = Vec::new();
    for host in hosts {
        match remote_json(client, &host, "agent list").await {
            Ok(Some(Value::Array(agents))) => {
                out.extend(agents.into_iter().map(|mut agent| {
                    agent["host"] = json!(host);
                    agent
                }));
            }
            Ok(_) => {}
            Err(err) => out.push(json!({
                "host": host,
                "status": "unreachable",
                "detail": format!("{err:#}"),
            })),
        }
    }
    Ok(out)
}

#[derive(Serialize)]
pub struct Orphan {
    pub name: String,
    pub status: String,
    pub size: String,
}

/// Machines on the account that no ledger in the reachable tree tracks. The
/// second value is true when part of the tree could not be searched.
pub async fn orphans(client: &mut boxd::Client) -> Result<(Vec<Orphan>, bool)> {
    let tree = list(client).await?;
    let nodes = deepest_first(&tree);
    let partial = nodes.iter().any(|node| {
        node.unreachable.is_some() || (node.status != "running" && node.status != "gone")
    });
    let mut tracked: HashSet<&str> = nodes.iter().map(|node| node.name.as_str()).collect();
    let here = store::host_name();
    if let Some(here) = &here {
        tracked.insert(here);
    }
    let orphans = client
        .list_vms()
        .await?
        .vms
        .into_iter()
        .filter(|vm| !tracked.contains(vm.name.as_str()))
        .map(|vm| Orphan {
            size: machine::size_label(vm.vcpu, vm.memory_bytes),
            name: vm.name,
            status: vm.status,
        })
        .collect();
    Ok((orphans, partial))
}

#[derive(Serialize, Default)]
pub struct Synced {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<Value>,
}

/// Makes every running machine in the tree a saved Herdr machine here, and
/// removes plugin-made profiles whose machine no longer exists.
pub async fn sync(client: &mut boxd::Client) -> Result<Synced> {
    if !herdr::available() {
        return Err(herdr::missing());
    }
    herdr::require_min()?;
    let tree = list(client).await?;
    let here = store::host_name();
    let mut synced = Synced::default();
    for node in deepest_first(&tree) {
        if node.status != "running" || here.as_deref() == Some(node.name.as_str()) {
            continue;
        }
        if herdr::profile_id(&node.name)?.is_some() {
            continue;
        }
        ui::step(format!("adding {} to Herdr", node.name));
        match machine::attach(client, &node.name).await {
            Ok(_) => synced.added.push(node.name.clone()),
            Err(err) => {
                ui::warn(format!("could not add {}: {err:#}", node.name));
                synced
                    .failed
                    .push(json!({ "machine": node.name, "error": format!("{err:#}") }));
            }
        }
    }
    let live: HashSet<String> = client
        .list_vms()
        .await?
        .vms
        .into_iter()
        .map(|vm| vm.name)
        .collect();
    for profile in herdr::profiles()? {
        if !profile.is_for(&profile.label) || live.contains(&profile.label) {
            continue;
        }
        herdr::remove_profile(&profile.label)?;
        let _ = ssh::remove(&profile.label);
        synced.removed.push(profile.label);
    }
    Ok(synced)
}

/// `sync` for the end of other commands: problems are warnings.
pub async fn sync_best_effort(client: &mut boxd::Client) {
    if !herdr::available() {
        return;
    }
    if let Err(err) = sync(client).await {
        ui::warn(format!("could not sync Herdr machines: {err:#}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, children: Vec<Listed>) -> Listed {
        Listed {
            name: name.into(),
            status: "running".into(),
            size: None,
            in_herdr: false,
            agents: 0,
            parent: String::new(),
            children,
            unreachable: None,
            manages: false,
        }
    }

    #[test]
    fn deepest_first_puts_children_before_parents() {
        let tree = vec![
            node(
                "a",
                vec![node("b", vec![node("c", vec![])]), node("d", vec![])],
            ),
            node("e", vec![]),
        ];
        let order: Vec<&str> = deepest_first(&tree)
            .into_iter()
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(order, ["c", "b", "d", "a", "e"]);
    }

    #[test]
    fn listed_reads_old_flat_output_and_nests_children() {
        let old: Vec<Listed> = serde_json::from_str(
            r#"[{"name":"a","status":"running","size":"1x4","in_herdr":true,"agents":0}]"#,
        )
        .unwrap();
        assert!(old[0].parent.is_empty() && old[0].children.is_empty());
        let text = serde_json::to_string(&node("a", vec![node("b", vec![])])).unwrap();
        assert!(text.contains(r#""children":[{"name":"b""#));
        assert!(!text.contains("manages"));
    }
}
