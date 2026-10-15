//! Machine lifecycle: create from the snapshot, attach to Herdr, destroy.
//! A start that fails or is interrupted after the machine exists destroys it
//! again, so a paid machine is never left behind half set up.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;

use crate::boxd::{self, proto::GetVmResponse};
use crate::copy::{self, Packed};
use crate::{herdr, ssh, store, ui, Coded};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub label: &'static str,
    pub vcpu: u32,
    pub memory_bytes: u64,
}

const GIB: u64 = 1024 * 1024 * 1024;
pub const SIZES: [Size; 4] = [
    Size {
        label: "1x4",
        vcpu: 1,
        memory_bytes: 4 * GIB,
    },
    Size {
        label: "2x8",
        vcpu: 2,
        memory_bytes: 8 * GIB,
    },
    Size {
        label: "4x16",
        vcpu: 4,
        memory_bytes: 16 * GIB,
    },
    Size {
        label: "8x32",
        vcpu: 8,
        memory_bytes: 32 * GIB,
    },
];
pub const DEFAULT_SIZE: Size = SIZES[1];

pub fn parse_size(raw: &str) -> Result<Size> {
    let raw = raw.trim().trim_end_matches(['G', 'g']);
    SIZES
        .into_iter()
        .find(|size| size.label == raw)
        .ok_or_else(|| {
            Coded::new(
                "usage",
                format!("size must be 1x4, 2x8, 4x16, or 8x32, not {raw}"),
            )
            .into()
        })
}

pub fn size_label(vcpu: u32, memory_bytes: u64) -> String {
    format!("{vcpu}x{}", memory_bytes / GIB)
}

pub fn is_machine_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 63
        && matches!(chars.next(), Some('a'..='z'))
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        && !name.ends_with('-')
}

pub fn check_machine_name(name: &str) -> Result<()> {
    if is_machine_name(name) {
        return Ok(());
    }
    bail!(Coded::new(
        "usage",
        format!(
            "machine name {name:?} must start with a-z and use only a-z, 0-9, and '-' (max 63)"
        ),
    ));
}

pub async fn connect() -> Result<boxd::Client> {
    let dirs = store::dirs();
    let key = store::api_key(&dirs);
    let token = boxd::load_token(key.as_deref())?;
    boxd::Client::connect(&token).await
}

pub struct Plan {
    pub name: Option<String>,
    pub size: Size,
    pub snapshot: String,
    pub packed: Option<Packed>,
}

#[derive(Serialize)]
pub struct Ready {
    pub machine: String,
    pub size: String,
    /// The Herdr profile id; absent when Herdr is not installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub workdir: String,
}

/// A machine that exists but is not fully set up yet; `rollback` removes it.
pub struct Created {
    name: String,
    profile: bool,
}

/// Resolves on SIGINT, SIGTERM, or SIGHUP (the popup closing).
pub async fn interrupted() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut int), Ok(mut term), Ok(mut hup)) = (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = int.recv() => {}
        _ = term.recv() => {}
        _ = hup.recv() => {}
    }
}

pub async fn start(client: &mut boxd::Client, plan: &Plan) -> Result<Ready> {
    let mut created = None;
    let result = tokio::select! {
        result = provision(client, plan, &mut created) => result,
        () = interrupted() => Err(Coded::new("cancelled", "interrupted").into()),
    };
    if result.is_err() {
        if let Some(created) = created {
            rollback(client, created).await;
        }
    }
    result
}

/// The un-guarded start. Callers that run their own interrupt handling pass
/// `created` so they can roll back a half-made machine themselves.
pub async fn provision(
    client: &mut boxd::Client,
    plan: &Plan,
    created: &mut Option<Created>,
) -> Result<Ready> {
    // Everything that can fail cheaply runs before the machine exists.
    let herdr = herdr::available();
    if herdr {
        ssh::identity_file()?;
    }
    client.check_snapshot(&plan.snapshot).await?;
    let name = match &plan.name {
        Some(name) => {
            check_machine_name(name)?;
            if client.get_vm(name).await?.is_some() {
                bail!(Coded::new(
                    "already_exists",
                    format!("a machine named {name} already exists; pick another name"),
                ));
            }
            name.clone()
        }
        None => client.suggest_name().await?,
    };

    ui::step(format!("creating {name} from {}", plan.snapshot));
    let name = client.create_from_snapshot(&plan.snapshot, &name).await?;
    *created = Some(Created {
        name: name.clone(),
        profile: false,
    });
    store::remember(&store::dirs(), &name)?;
    let mut vm = client.wait_running(&name, Duration::from_secs(180)).await?;

    if vm.vcpu != plan.size.vcpu || vm.memory_bytes != plan.size.memory_bytes {
        ui::step(format!("resizing to {} (reboots once)", plan.size.label));
        client
            .resize(&name, plan.size.vcpu, plan.size.memory_bytes)
            .await?;
        vm = client.wait_running(&name, Duration::from_secs(180)).await?;
    } else {
        // The restored Herdr server still has shells that cached the
        // snapshot's hostname. Restarting it is enough; `herdr machine add`
        // starts it again with fresh shells. Cheaper than a reboot.
        let _ = client
            .exec(&name, "herdr server stop >/dev/null 2>&1")
            .await;
    }

    // Without Herdr there is nothing that needs SSH: exec and uploads go
    // through the boxd API, and boxd's daemon still writes the alias.
    let profile = if herdr {
        ui::step("waiting for SSH");
        connect_ssh(client, &name, &vm).await?;
        ui::step(format!("adding {name} to Herdr"));
        let profile = herdr::ensure_profile(&name)?;
        if let Some(created) = created.as_mut() {
            created.profile = true;
        }
        Some(profile)
    } else {
        ui::note("Herdr is not installed here, so the machine is not added to a sidebar");
        None
    };

    let workdir = match &plan.packed {
        Some(packed) => copy::push(client, &name, packed).await?,
        None => "/home/boxd".to_owned(),
    };
    if plan.packed.is_some() {
        if let Some(profile) = &profile {
            open_project_shell(profile, &workdir).await;
        }
    }
    *created = None;
    Ok(Ready {
        machine: name,
        size: size_label(vm.vcpu, vm.memory_bytes),
        profile,
        workdir,
    })
}

async fn connect_ssh(client: &mut boxd::Client, name: &str, vm: &GetVmResponse) -> Result<()> {
    let domain = if vm.access_domain.is_empty() {
        "boxd.sh"
    } else {
        &vm.access_domain
    };
    let host = format!("{name}.{domain}");
    let host_key = client.ssh_host_key().await?;
    // boxd's daemon usually writes the alias within a few seconds of creation.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while daemon_manages_ssh() && tokio::time::Instant::now() < deadline {
        if ssh::resolves_to(name, &host, vm.ssh_port) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if ssh::ensure_alias(name, &host, vm.ssh_port, &host_key)? {
        ssh::warn_fallback(name);
    }
    ssh::wait_ready(name, Duration::from_secs(120)).await
}

fn daemon_manages_ssh() -> bool {
    std::env::var("HOME")
        .ok()
        .and_then(|home| std::fs::read_to_string(format!("{home}/.ssh/config")).ok())
        .is_some_and(|text| text.contains("# BEGIN boxd"))
}

pub async fn rollback(client: &mut boxd::Client, created: Created) {
    let name = created.name;
    ui::warn(format!("cleaning up {name}"));
    if created.profile {
        if let Err(err) = herdr::remove_profile(&name) {
            ui::warn(format!("could not remove the Herdr profile: {err:#}"));
        }
    }
    let _ = ssh::remove(&name);
    match client.destroy(&name).await {
        Ok(_) => {
            let _ = store::forget(&store::dirs(), &name);
            ui::done(format!("destroyed {name}"));
        }
        Err(err) => ui::fail(format!(
            "could not destroy {name}: {err:#}. Run `herdr-boxd destroy {name}`"
        )),
    }
}

/// Brings an existing plugin machine back to a usable state: awake, reachable
/// over SSH, and in Herdr with its server answering.
pub async fn attach(client: &mut boxd::Client, name: &str) -> Result<String> {
    if !herdr::available() {
        return Err(herdr::missing());
    }
    let vm = client.ensure_running(name).await?;
    connect_ssh(client, name, &vm).await?;
    let profile = herdr::ensure_profile(name)?;
    herdr::wait_reachable(&profile, Duration::from_secs(60)).await?;
    Ok(profile)
}

pub fn require_owned(name: &str) -> Result<()> {
    if store::owns(&store::dirs(), name) {
        return Ok(());
    }
    bail!(Coded::new(
        "not_owned",
        format!("{name} was not created by herdr-boxd; it only manages its own machines"),
    ));
}

/// Destroys first, then cleans local state, so a failed destroy leaves
/// everything in place to retry.
pub async fn destroy(client: &mut boxd::Client, name: &str) -> Result<bool> {
    let existed = client.destroy(name).await?;
    if let Err(err) = herdr::remove_profile(name) {
        ui::warn(format!("could not remove the Herdr profile: {err:#}"));
    }
    if let Err(err) = ssh::remove(name) {
        ui::warn(format!("could not clean ~/.ssh/config: {err}"));
    }
    store::forget(&store::dirs(), name)?;
    Ok(existed)
}

#[derive(Serialize, Clone)]
pub struct Listed {
    pub name: String,
    /// boxd status, or "gone" when the machine no longer exists.
    pub status: String,
    pub size: Option<String>,
    pub in_herdr: bool,
    pub agents: usize,
}

pub async fn list(client: &mut boxd::Client) -> Result<Vec<Listed>> {
    let dirs = store::dirs();
    let vms: HashMap<String, GetVmResponse> = client
        .list_vms()
        .await?
        .vms
        .into_iter()
        .map(|vm| (vm.name.clone(), vm))
        .collect();
    let profiles = herdr::profiles().unwrap_or_default();
    let agents = store::agents(&dirs);
    Ok(store::machines(&dirs)
        .into_iter()
        .map(|name| {
            let vm = vms.get(&name);
            Listed {
                status: vm.map_or("gone".into(), |vm| vm.status.clone()),
                size: vm.map(|vm| size_label(vm.vcpu, vm.memory_bytes)),
                in_herdr: profiles.iter().any(|p| p.is_for(&name)),
                agents: agents.iter().filter(|a| a.machine == name).count(),
                name,
            }
        })
        .collect())
}

pub async fn open_project_shell(profile: &str, workdir: &str) {
    if profile.is_empty() || is_home_cwd(workdir) {
        return;
    }
    if let Err(err) = replace_home_shell(profile, workdir).await {
        ui::warn(format!("could not open a shell in {workdir}: {err:#}"));
    }
}

async fn replace_home_shell(profile: &str, workdir: &str) -> Result<()> {
    ui::step(format!("opening a shell in {workdir}"));
    herdr::wait_reachable(profile, Duration::from_secs(60)).await?;
    let label = workdir.rsplit('/').next().unwrap_or("code");
    let created = herdr::remote(
        profile,
        &[
            "workspace",
            "create",
            "--cwd",
            workdir,
            "--label",
            label,
            "--no-focus",
        ],
    )
    .await?;
    let keep = created["result"]["workspace"]["workspace_id"]
        .as_str()
        .context("herdr workspace create returned no workspace id")?;
    let listed = herdr::remote(profile, &["pane", "list"]).await?;
    let panes = listed["result"]["panes"]
        .as_array()
        .context("herdr pane list returned no panes")?;
    for id in home_workspaces_to_close(panes, keep) {
        if let Err(err) = herdr::remote(profile, &["workspace", "close", &id]).await {
            ui::warn(format!("could not close the home shell {id}: {err:#}"));
        }
    }
    Ok(())
}

/// Workspaces whose panes are all still in `/home/boxd`, excluding `keep`.
fn home_workspaces_to_close(panes: &[Value], keep: &str) -> Vec<String> {
    let mut all_home: HashMap<&str, bool> = HashMap::new();
    for pane in panes {
        let Some(id) = pane.get("workspace_id").and_then(Value::as_str) else {
            continue;
        };
        if id == keep {
            continue;
        }
        let home = pane
            .get("cwd")
            .and_then(Value::as_str)
            .is_some_and(is_home_cwd);
        all_home
            .entry(id)
            .and_modify(|flag| *flag &= home)
            .or_insert(home);
    }
    let mut ids: Vec<String> = all_home
        .into_iter()
        .filter(|(_, home)| *home)
        .map(|(id, _)| id.to_owned())
        .collect();
    ids.sort();
    ids
}

fn is_home_cwd(cwd: &str) -> bool {
    cwd.trim_end_matches('/') == "/home/boxd"
}

pub async fn save_snapshot(client: &mut boxd::Client, vm: &str, snapshot: &str) -> Result<u64> {
    client.ensure_running(vm).await?;
    ui::step(format!("saving {vm} as snapshot {snapshot}"));
    client.save_snapshot(vm, snapshot).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_name_accepts_herdr_box() {
        assert!(is_machine_name("herdr-box"));
    }

    #[test]
    fn machine_name_rejects_uppercase_and_shell_text() {
        assert!(!is_machine_name("Herdr"));
        assert!(!is_machine_name("a;b"));
        assert!(!is_machine_name("box-"));
        assert!(!is_machine_name(&"a".repeat(64)));
    }

    #[test]
    fn parse_size_accepts_labels_with_or_without_g() {
        assert_eq!(parse_size("2x8").unwrap(), DEFAULT_SIZE);
        assert_eq!(parse_size("4x16G").unwrap().vcpu, 4);
        assert!(parse_size("3x12").is_err());
    }

    #[test]
    fn only_all_home_workspaces_are_closed() {
        let panes = serde_json::json!([
            {"workspace_id": "w1", "cwd": "/home/boxd"},
            {"workspace_id": "w1", "cwd": "/home/boxd/"},
            {"workspace_id": "w2", "cwd": "/home/boxd/app"},
            {"workspace_id": "w3", "cwd": "/home/boxd"},
            {"workspace_id": "w4", "cwd": "/home/boxd"},
            {"workspace_id": "w4", "cwd": "/tmp"},
            {"workspace_id": "w5"}
        ]);
        assert_eq!(
            home_workspaces_to_close(panes.as_array().unwrap(), "w3"),
            vec!["w1".to_owned()]
        );
    }
}
