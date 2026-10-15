//! Machine lifecycle: create from the snapshot, attach to Herdr, destroy.
//! A start that fails or is interrupted after the machine exists destroys it
//! again, so a paid machine is never left behind half set up.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

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
    boxd::Client::connect().await
}

pub struct Plan {
    pub name: Option<String>,
    pub size: Size,
    pub snapshot: String,
    pub packed: Option<Packed>,
    /// Headless agents use the boxd API only.
    pub attach_herdr: bool,
}

#[derive(Serialize)]
pub struct Ready {
    pub machine: String,
    pub size: String,
    /// The Herdr profile id; absent when Herdr is not installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub workdir: String,
    /// Workspaces restored from the snapshot, seen right after
    /// `herdr machine add`; closed later only if still untouched.
    #[serde(skip)]
    pub home_workspaces: Vec<String>,
}

/// A machine that exists but is not fully set up yet; `rollback` removes it.
pub struct Created {
    name: String,
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
    if plan.packed.is_some() && herdr::available() {
        herdr::require_min()?;
    }
    let mut created = None;
    let result = tokio::select! {
        result = provision(client, plan, &mut created) => result,
        () = interrupted() => Err(Coded::new("cancelled", "interrupted").into()),
    };
    let ready = match result {
        Ok(ready) => ready,
        Err(err) => {
            if let Some(created) = created {
                rollback(client, created).await;
            }
            return Err(err);
        }
    };
    if plan.packed.is_some() {
        if let Some(profile) = &ready.profile {
            tokio::select! {
                () = open_project_shell(profile, &ready.workdir, &ready.home_workspaces) => {}
                () = interrupted() => {
                    ui::warn(format!(
                        "interrupted while opening a shell in {}; {} is ready",
                        ready.workdir, ready.machine
                    ));
                }
            }
        }
    }
    Ok(ready)
}

/// The un-guarded start. Callers that run their own interrupt handling pass
/// `created` so they can roll back a half-made machine themselves.
pub async fn provision(
    client: &mut boxd::Client,
    plan: &Plan,
    created: &mut Option<Created>,
) -> Result<Ready> {
    // Everything that can fail cheaply runs before the machine exists.
    let herdr = plan.attach_herdr && herdr::available();
    if herdr {
        ensure_identity()?;
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
    *created = Some(Created { name: name.clone() });
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
        Some(herdr::ensure_profile(&name).await?)
    } else {
        ui::note("Herdr is not installed here, so the machine is not added to a sidebar");
        None
    };

    // Before the user can touch them: only these may be tidied away later.
    let home_workspaces = match (&profile, &plan.packed) {
        (Some(profile), Some(_)) => workspace_ids(profile).await,
        _ => Vec::new(),
    };
    let workdir = match &plan.packed {
        Some(packed) => copy::push(client, &name, packed).await?,
        None => "/home/boxd".to_owned(),
    };
    *created = None;
    Ok(Ready {
        machine: name,
        size: size_label(vm.vcpu, vm.memory_bytes),
        profile,
        workdir,
        home_workspaces,
    })
}

/// The SSH key to use. Outside a VM that is boxd's own key; inside one, the
/// plugin's key, made once. Returns the public key line inside a VM.
pub fn ensure_identity() -> Result<Option<String>> {
    if !boxd::in_vm() {
        ssh::identity_file()?;
        return Ok(None);
    }
    let host = store::host_name().unwrap_or_else(|| "boxd-vm".into());
    let pubkey = ssh::generate_key(&ssh::vm_key()?, &format!("herdr-boxd@{host}"))?;
    Ok(Some(pubkey))
}

/// Run in the machine over exec: trust our key, start a key-only sshd (boxd
/// machines ship it masked), and print the host key to pin.
const SSHD_SETUP: &str = r#"set -e
mkdir -p ~/.ssh && chmod 700 ~/.ssh
touch ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys
grep -qxF "$KEY" ~/.ssh/authorized_keys || printf '%s\n' "$KEY" >> ~/.ssh/authorized_keys
if ! systemctl is-active --quiet ssh.service; then
  printf 'PasswordAuthentication no\nKbdInteractiveAuthentication no\nPermitRootLogin no\n' |
    sudo tee /etc/ssh/sshd_config.d/herdr-boxd.conf >/dev/null
  sudo systemctl unmask ssh.service >/dev/null 2>&1 || true
  sudo systemctl enable --now ssh.service >/dev/null 2>&1
fi
cut -d' ' -f1,2 /etc/ssh/ssh_host_ed25519_key.pub"#;

async fn connect_ssh(client: &mut boxd::Client, name: &str, vm: &GetVmResponse) -> Result<()> {
    if let Some(pubkey) = ensure_identity()? {
        return connect_private(client, name, &pubkey).await;
    }
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

/// Inside a VM, boxd's SSH gateway is closed to us: it only takes keys linked
/// by an interactive login, which a VM token cannot do. So the machine gets
/// our key and an sshd over the authenticated exec stream, and we reach it on
/// the private network, where `<name>.boxd` resolves to its address. The host
/// key is pinned from that same exec, before the first connection.
async fn connect_private(client: &mut boxd::Client, name: &str, pubkey: &str) -> Result<()> {
    let script = format!("KEY={}\n{SSHD_SETUP}", crate::agent::sh_quote(pubkey));
    // Right after a boot (a resize reboots) systemd may not answer yet.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let host_key = loop {
        match client.exec(name, &script).await?.check("setting up SSH") {
            Ok(output) => break output.stdout_text(),
            Err(err) if tokio::time::Instant::now() >= deadline => return Err(err),
            Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    };
    let host_key = host_key.trim();
    if !host_key.starts_with("ssh-") {
        bail!("{name} printed no SSH host key");
    }
    ssh::ensure_alias(name, &ssh::alias(name), 22, host_key)?;
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
    if let Err(err) = herdr::remove_profile(&name) {
        ui::warn(format!("could not remove the Herdr profile: {err:#}"));
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
    herdr::require_min()?;
    let vm = client.ensure_running(name).await?;
    connect_ssh(client, name, &vm).await?;
    let profile = herdr::ensure_profile(name).await?;
    herdr::wait_reachable(&profile, Duration::from_secs(60)).await?;
    Ok(profile)
}

pub fn require_owned(name: &str) -> Result<()> {
    if store::owns(&store::dirs(), name) {
        return Ok(());
    }
    bail!(Coded::new(
        "not_owned",
        format!(
            "{name} is not in this host's herdr-boxd ledger; `herdr-boxd list` shows which machine manages it"
        ),
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

/// Destroys what `name` created (`below`, from `tree::descendants`) deepest
/// first, then `name`. A failure stops before `name`, so a retry finds the
/// rest still tracked.
pub async fn destroy_tree(client: &mut boxd::Client, name: &str, below: &[Listed]) -> Result<bool> {
    for node in crate::tree::deepest_first(below) {
        ui::step(format!(
            "destroying {} (created by {})",
            node.name, node.parent
        ));
        client.destroy(&node.name).await?;
        // `sync` may have added it here too.
        if let Err(err) = herdr::remove_profile(&node.name) {
            ui::warn(format!("could not remove the Herdr profile: {err:#}"));
        }
        let _ = ssh::remove(&node.name);
    }
    destroy(client, name).await
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Listed {
    pub name: String,
    /// boxd status, or "gone" when the machine no longer exists.
    pub status: String,
    pub size: Option<String>,
    /// Whether the host that tracks this machine has it in Herdr.
    pub in_herdr: bool,
    pub agents: usize,
    /// The host that created it; empty at the root.
    #[serde(default)]
    pub parent: String,
    /// Machines this one created, found by running herdr-boxd inside it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Listed>,
    /// Why its own machines could not be listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unreachable: Option<String>,
    /// herdr-boxd answered inside it, so it can host agents of its own.
    #[serde(skip)]
    pub manages: bool,
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
    let here = store::host_name().unwrap_or_default();
    Ok(store::entries(&dirs)
        .into_iter()
        .map(|entry| {
            let name = entry.name;
            let vm = vms.get(&name);
            Listed {
                status: vm.map_or("gone".into(), |vm| vm.status.clone()),
                size: vm.map(|vm| size_label(vm.vcpu, vm.memory_bytes)),
                in_herdr: profiles.iter().any(|p| p.is_for(&name)),
                agents: agents.iter().filter(|a| a.machine == name).count(),
                parent: if entry.parent.is_empty() {
                    here.clone()
                } else {
                    entry.parent
                },
                children: Vec::new(),
                unreachable: None,
                manages: false,
                name,
            }
        })
        .collect())
}

/// Opens the project's workspace, then closes the snapshot's leftover home
/// workspaces that nobody has used.
pub async fn open_project_shell(profile: &str, workdir: &str, home_workspaces: &[String]) {
    if profile.is_empty() {
        return;
    }
    match focus_project_shell(profile, workdir).await {
        Ok(project) => {
            for id in home_workspaces.iter().filter(|id| **id != project) {
                close_if_untouched(profile, id).await;
            }
        }
        Err(err) => ui::warn(format!("could not open a shell in {workdir}: {err:#}")),
    }
}

async fn focus_project_shell(profile: &str, workdir: &str) -> Result<String> {
    ui::step(format!("opening a shell in {workdir}"));
    herdr::wait_reachable(profile, Duration::from_secs(60)).await?;
    let label = workdir
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(workdir);
    let (workspace, _) = herdr::create_workspace(profile, workdir, label, true).await?;
    Ok(workspace)
}

/// Workspace ids on the machine now; empty if Herdr does not answer.
async fn workspace_ids(profile: &str) -> Vec<String> {
    let Ok(reply) =
        herdr::remote_timeout(profile, &["workspace", "list"], Duration::from_secs(20)).await
    else {
        return Vec::new();
    };
    reply["result"]["workspaces"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| w["workspace_id"].as_str().map(str::to_owned))
        .collect()
}

/// Closes `workspace` only if it is still exactly what the snapshot restored:
/// one tab, one pane in /home/boxd, no agent, and the shell idle. Any doubt or
/// error leaves it open.
async fn close_if_untouched(profile: &str, workspace: &str) {
    let limit = Duration::from_secs(20);
    let Ok(info) = herdr::remote_timeout(profile, &["workspace", "get", workspace], limit).await
    else {
        return;
    };
    let Ok(panes) =
        herdr::remote_timeout(profile, &["pane", "list", "--workspace", workspace], limit).await
    else {
        return;
    };
    let Some(pane) = sole_home_pane(&info["result"]["workspace"], &panes["result"]["panes"]) else {
        return;
    };
    let Ok(process) =
        herdr::remote_timeout(profile, &["pane", "process-info", "--pane", &pane], limit).await
    else {
        return;
    };
    if !shell_idle(&process["result"]["process_info"]) {
        return;
    }
    if herdr::remote_timeout(profile, &["workspace", "close", workspace], limit)
        .await
        .is_ok()
    {
        ui::step(format!("closed the unused home workspace {workspace}"));
    }
}

const HOME: &str = "/home/boxd";

/// The pane id when the workspace has one tab and one pane, in the home
/// directory, with no agent.
fn sole_home_pane(workspace: &serde_json::Value, panes: &serde_json::Value) -> Option<String> {
    let panes = panes.as_array()?;
    if workspace["tab_count"] != 1 || workspace["pane_count"] != 1 || panes.len() != 1 {
        return None;
    }
    let pane = &panes[0];
    let no_agent = pane["agent"].as_str().is_none_or(str::is_empty)
        && pane["agent_status"].as_str().is_none_or(|s| s == "unknown");
    let home = pane["cwd"] == HOME && pane["foreground_cwd"].as_str().is_none_or(|c| c == HOME);
    (no_agent && home)
        .then(|| pane["pane_id"].as_str().map(str::to_owned))
        .flatten()
}

/// True when the only foreground process is the pane's own shell.
fn shell_idle(info: &serde_json::Value) -> bool {
    let Some(shell) = info["shell_pid"].as_u64() else {
        return false;
    };
    let Some(procs) = info["foreground_processes"].as_array() else {
        return false;
    };
    info["foreground_process_group_id"].as_u64() == Some(shell)
        && !procs.is_empty()
        && procs.iter().all(|p| p["pid"].as_u64() == Some(shell))
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
    fn home_workspace_is_closed_only_when_untouched() {
        use serde_json::json;
        let ws = json!({"tab_count": 1, "pane_count": 1});
        let pane = json!({"pane_id": "w2:p1", "cwd": HOME, "foreground_cwd": HOME, "agent_status": "unknown"});
        assert_eq!(
            sole_home_pane(&ws, &json!([pane])).as_deref(),
            Some("w2:p1")
        );
        let mut moved = pane.clone();
        moved["cwd"] = json!("/home/boxd/app");
        assert!(sole_home_pane(&ws, &json!([moved])).is_none());
        let mut agent = pane.clone();
        agent["agent"] = json!("claude");
        assert!(sole_home_pane(&ws, &json!([agent])).is_none());
        let split = json!({"tab_count": 1, "pane_count": 2});
        assert!(sole_home_pane(&split, &json!([pane, pane])).is_none());

        let idle = json!({"shell_pid": 439, "foreground_process_group_id": 439,
            "foreground_processes": [{"pid": 439, "name": "bash"}]});
        assert!(shell_idle(&idle));
        let busy = json!({"shell_pid": 439, "foreground_process_group_id": 803,
            "foreground_processes": [{"pid": 803, "name": "sleep"}]});
        assert!(!shell_idle(&busy));
        assert!(!shell_idle(&json!({})));
    }

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
}
