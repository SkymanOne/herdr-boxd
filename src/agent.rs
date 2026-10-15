//! Subagents inside plugin machines, for other agents to spawn and monitor.
//!
//! Pane mode runs the agent in a Herdr pane on the machine: it shows up in the
//! user's sidebar and Herdr tracks idle/working/blocked/done. Headless mode
//! runs `claude -p` or `codex exec` detached, logging to
//! ~/.herdr-boxd/agents/<name> in the machine.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::machine::{self, Plan, Size};
use crate::store::{self, Agent, Mode};
use crate::{boxd, copy, herdr, ui, Coded};

pub const HEADLESS_KINDS: [&str; 2] = ["claude", "codex"];

pub struct SpawnOpts {
    pub name: Option<String>,
    pub kind: String,
    pub machine: Option<String>,
    pub size: Size,
    pub cwd: Option<PathBuf>,
    pub workdir: Option<String>,
    pub headless: bool,
    pub prompt: Option<String>,
    pub wait: bool,
    pub timeout_ms: Option<u64>,
    pub args: Vec<String>,
}

pub fn is_agent_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 32
        && matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn agent_dir(name: &str) -> String {
    format!("/home/boxd/.herdr-boxd/agents/{name}")
}

pub async fn spawn(opts: SpawnOpts) -> Result<Value> {
    let dirs = store::dirs();
    let name = opts
        .name
        .clone()
        .unwrap_or_else(|| format!("{}-{}", opts.kind, store::random_hex(2)));
    if !is_agent_name(&name) {
        bail!(Coded::new(
            "usage",
            format!("agent name {name:?} must match [a-z][a-z0-9_-]{{0,31}}"),
        ));
    }
    if opts.headless && !HEADLESS_KINDS.contains(&opts.kind.as_str()) {
        bail!(Coded::new(
            "usage",
            format!(
                "--headless supports {}, not {}",
                HEADLESS_KINDS.join(" and "),
                opts.kind
            ),
        ));
    }
    if !opts.headless {
        if !herdr::available() {
            return Err(herdr::missing());
        }
        herdr::require_min()?;
    }
    if opts.headless && opts.prompt.is_none() {
        bail!(Coded::new(
            "usage",
            "--headless needs --prompt or --prompt-file"
        ));
    }
    if let Some(machine) = &opts.machine {
        machine::require_owned(machine)?;
    }
    let workdir = opts.workdir.as_deref().map(normalize_workdir).transpose()?;
    let mut agent = Agent {
        name: name.clone(),
        machine: opts.machine.clone().unwrap_or_default(),
        kind: opts.kind.clone(),
        mode: if opts.headless {
            Mode::Headless
        } else {
            Mode::Pane
        },
        workdir: String::new(),
        workspace: None,
        session: None,
        args: opts.args.clone(),
        owns_machine: opts.machine.is_none(),
        created: store::now_secs(),
    };
    // Claim the name before any slow work so parallel spawns cannot collide.
    if !store::claim_agent(&dirs, &agent)? {
        bail!(Coded::new(
            "already_exists",
            format!("an agent named {name} already exists; stop it or pick another --name"),
        ));
    }
    let packed = match opts.cwd.as_deref().map(copy::pack).transpose() {
        Ok(packed) => packed,
        Err(err) => {
            let _ = store::forget_agent(&dirs, &name);
            return Err(err);
        }
    };
    // Unpacking over a directory another agent is working in would clobber
    // its uncommitted edits and .git.
    if let (Some(machine), Some(packed)) = (&opts.machine, &packed) {
        let dir = packed.remote_dir();
        if let Some(other) = store::agents(&dirs)
            .into_iter()
            .find(|a| a.name != name && &a.machine == machine && a.workdir == dir)
        {
            let _ = store::forget_agent(&dirs, &name);
            bail!(Coded::new(
                "workdir_in_use",
                format!(
                    "agent {} works in {dir} on {machine}; pass `--workdir {dir}` to share it instead of copying over it",
                    other.name
                ),
            ));
        }
    }
    let mut client = match machine::connect().await {
        Ok(client) => client,
        Err(err) => {
            let _ = store::forget_agent(&dirs, &name);
            return Err(err);
        }
    };
    // One interrupt guard for the whole spawn. `created` holds a machine that
    // exists but is not set up yet, so an interrupt there still destroys it.
    let mut created = None;
    let launched = tokio::select! {
        result = launch(&mut client, &opts, &mut agent, workdir, packed, &mut created) => result,
        () = machine::interrupted() => Err(Coded::new("cancelled", "interrupted").into()),
    };
    let (status, profile) = match launched {
        Ok(launched) => launched,
        Err(err) => {
            ui::warn(format!("spawn failed, cleaning up {name}"));
            if let Some(created) = created {
                machine::rollback(&mut client, created).await;
            }
            cleanup_failed_spawn(&mut client, &agent).await;
            return Err(err);
        }
    };
    // The agent is running now. A prompt or wait that fails, times out, or is
    // interrupted from here on must not tear down the machine or its work.
    store::save_agent(&dirs, &agent)?;
    let result = tokio::select! {
        result = follow_up(&mut client, &opts, &agent, &profile, status) => result,
        () = machine::interrupted() => Err(Coded::new("cancelled", "interrupted").into()),
    };
    let status = result.with_context(|| {
        format!(
            "{name} was spawned on {} and keeps running; check it with `herdr-boxd agent status {name}`",
            agent.machine
        )
    })?;
    Ok(view(&agent, &status))
}

async fn follow_up(
    client: &mut boxd::Client,
    opts: &SpawnOpts,
    agent: &Agent,
    profile: &str,
    status: Status,
) -> Result<Status> {
    match (&agent.mode, &opts.prompt) {
        (Mode::Pane, Some(prompt)) if status.status != "blocked" => {
            pane_prompt(profile, &agent.name, prompt, opts.wait, opts.timeout_ms).await
        }
        (Mode::Headless, _) if opts.wait => wait_headless(client, agent, opts.timeout_ms).await,
        _ => Ok(status),
    }
}

/// Remote paths must be absolute; `~` means /home/boxd.
fn normalize_workdir(dir: &str) -> Result<String> {
    let dir = match dir.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("/home/boxd{rest}"),
        _ => dir.to_owned(),
    };
    if !dir.starts_with('/') {
        bail!(Coded::new(
            "usage",
            format!("--workdir must be an absolute path or start with ~/, not {dir:?}"),
        ));
    }
    let trimmed = dir.trim_end_matches('/');
    Ok(if trimmed.is_empty() {
        "/".into()
    } else {
        trimmed.into()
    })
}

async fn launch(
    client: &mut boxd::Client,
    opts: &SpawnOpts,
    agent: &mut Agent,
    workdir: Option<String>,
    packed: Option<copy::Packed>,
    created: &mut Option<machine::Created>,
) -> Result<(Status, String)> {
    let profile = match &opts.machine {
        // Headless agents only need the machine awake, not SSH or Herdr.
        Some(machine) if agent.mode == Mode::Headless => {
            client.ensure_running(machine).await?;
            String::new()
        }
        Some(machine) => machine::attach(client, machine).await?,
        None => {
            let plan = Plan {
                name: None,
                size: opts.size,
                snapshot: store::snapshot_name(&store::dirs()),
                packed: None,
            };
            let ready = machine::provision(client, &plan, created).await?;
            agent.machine = ready.machine;
            store::save_agent(&store::dirs(), agent)?;
            ready.profile.unwrap_or_default()
        }
    };
    agent.workdir = match &packed {
        Some(packed) => copy::push(client, &agent.machine, packed).await?,
        None => workdir.unwrap_or_else(|| "/home/boxd".into()),
    };
    client
        .exec(
            &agent.machine,
            &format!("mkdir -p {}", sh_quote(&agent.workdir)),
        )
        .await?
        .check("creating the work directory")?;
    let status = match agent.mode {
        Mode::Pane => spawn_pane(client, &profile, opts, agent).await?,
        Mode::Headless => spawn_headless(client, opts, agent).await?,
    };
    Ok((status, profile))
}

async fn cleanup_failed_spawn(client: &mut boxd::Client, agent: &Agent) {
    let dirs = store::dirs();
    if agent.owns_machine && !agent.machine.is_empty() {
        if let Err(err) = machine::destroy(client, &agent.machine).await {
            ui::fail(format!(
                "could not destroy {}: {err:#}. Run `herdr-boxd destroy {}`",
                agent.machine, agent.machine
            ));
        }
    } else if let (Some(workspace), Ok(Some(profile))) =
        (&agent.workspace, herdr::profile_id(&agent.machine))
    {
        let _ = herdr::remote(&profile, &["workspace", "close", workspace]).await;
    }
    let _ = store::forget_agent(&dirs, &agent.name);
}

async fn spawn_pane(
    client: &mut boxd::Client,
    profile: &str,
    opts: &SpawnOpts,
    agent: &mut Agent,
) -> Result<Status> {
    if agent.kind == "claude" {
        trust_claude_dir(client, &agent.machine, &agent.workdir).await;
    }
    ui::step(format!(
        "opening a Herdr workspace for {} on {}",
        agent.name, agent.machine
    ));
    let (workspace, pane) =
        herdr::create_workspace(profile, &agent.workdir, &agent.name, false).await?;
    agent.workspace = Some(workspace);
    store::save_agent(&store::dirs(), agent)?;

    ui::step(format!("starting {} in {pane}", agent.kind));
    let mut start = vec![
        "agent",
        "start",
        &agent.name,
        "--kind",
        &agent.kind,
        "--pane",
        &pane,
        "--timeout",
        "90000",
    ];
    if !opts.args.is_empty() {
        start.push("--");
        start.extend(opts.args.iter().map(String::as_str));
    }
    match herdr::remote(profile, &start).await {
        Ok(_) => {}
        Err(err) if code_of(&err) == Some("agent_not_ready") => {
            // Started but stuck on a dialog. Keep it so the caller can look
            // with `agent read` and answer with `agent keys`.
            ui::warn(format!(
                "{} is waiting on a startup dialog; see `herdr-boxd agent read {}`",
                agent.name, agent.name
            ));
            return Ok(Status::new("blocked"));
        }
        Err(err) => return Err(err),
    }
    Ok(Status::new("idle"))
}

async fn pane_prompt(
    profile: &str,
    name: &str,
    prompt: &str,
    wait: bool,
    timeout_ms: Option<u64>,
) -> Result<Status> {
    let timeout = timeout_ms.map(|ms| ms.to_string());
    let mut args = vec!["agent", "prompt", name, prompt];
    if wait {
        args.push("--wait");
    }
    if let Some(timeout) = &timeout {
        args.extend(["--timeout", timeout]);
    }
    let reply = herdr::remote(profile, &args).await?;
    if !wait {
        // The reply carries the state at submission time, usually still idle.
        return Ok(Status::new("submitted"));
    }
    Ok(Status::new(
        reply["result"]["agent"]["agent_status"]
            .as_str()
            .unwrap_or("unknown"),
    ))
}

/// Pre-accepts Claude's "trust this folder" dialog for the work directory. The
/// plugin copied it from the user's own project or the caller named it.
async fn trust_claude_dir(client: &mut boxd::Client, vm: &str, dir: &str) {
    let script = format!(
        r#"command -v python3 >/dev/null || exit 0
python3 - {dir} <<'EOF'
import json, os, sys
p = os.path.expanduser("~/.claude.json")
try:
    d = json.load(open(p))
except (OSError, ValueError):
    d = {{}}
d.setdefault("projects", {{}}).setdefault(sys.argv[1], {{}})["hasTrustDialogAccepted"] = True
tmp = p + ".herdr-boxd.tmp"
with open(tmp, "w") as f:
    json.dump(d, f, indent=2)
os.replace(tmp, p)
EOF"#,
        dir = sh_quote(dir)
    );
    if let Err(err) = client
        .exec(vm, &script)
        .await
        .and_then(|o| o.check("trust"))
    {
        ui::warn(format!("could not pre-trust {dir} for claude: {err:#}"));
    }
}

fn headless_command(agent: &Agent, resume: bool) -> String {
    let args: String = agent
        .args
        .iter()
        .map(|arg| format!(" {}", sh_quote(arg)))
        .collect();
    let session = agent.session.as_deref().map(sh_quote).unwrap_or_default();
    match (agent.kind.as_str(), resume) {
        ("codex", false) => format!(
            "codex exec --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -o \"$d/result.md\"{args} -"
        ),
        ("codex", true) => format!(
            "codex exec resume --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -o \"$d/result.md\"{args} {} -",
            if session.is_empty() { "--last".into() } else { session }
        ),
        (_, resume) => format!(
            "claude -p --output-format stream-json --verbose --dangerously-skip-permissions {} {session}{args}",
            if resume { "--resume" } else { "--session-id" }
        ),
    }
}

async fn spawn_headless(
    client: &mut boxd::Client,
    opts: &SpawnOpts,
    agent: &mut Agent,
) -> Result<Status> {
    if agent.kind == "claude" {
        agent.session = Some(store::uuid_v4());
    }
    let prompt = opts.prompt.as_deref().unwrap_or_default();
    run_headless(client, agent, prompt, false).await?;
    Ok(Status::new("working"))
}

/// Shell helper: `alive DIR` is true while that agent's run.sh is running.
/// Checking the command line, not just the pid, survives reboots and pid reuse.
const ALIVE: &str = r#"alive() { [ -s "$1/pid" ] && p=$(cat "$1/pid") && grep -qaF -- "$1/run.sh" "/proc/$p/cmdline" 2>/dev/null; }"#;

/// Exit code the launcher uses when another run holds the agent's lock.
const BUSY_EXIT: i32 = 75;

async fn run_headless(
    client: &mut boxd::Client,
    agent: &Agent,
    prompt: &str,
    resume: bool,
) -> Result<()> {
    let dir = agent_dir(&agent.name);
    ui::step(format!(
        "starting {} headless on {}",
        agent.kind, agent.machine
    ));
    client
        .exec(&agent.machine, &format!("mkdir -p {dir}"))
        .await?
        .check("creating the agent directory")?;
    // The prompt travels as a file and reaches the agent on stdin, so no
    // part of it is ever parsed by a shell.
    client
        .upload_bytes(
            &agent.machine,
            &format!("{dir}/prompt"),
            prompt.as_bytes().to_vec(),
        )
        .await?;
    let run = format!(
        "#!/bin/sh\nd=\"$1\"\necho $$ > \"$d/pid\"\ncd {workdir} || {{ echo 127 > \"$d/exit\"; exit 127; }}\n{cmd} < \"$d/prompt\" >> \"$d/out.jsonl\" 2>> \"$d/err.log\"\necho $? > \"$d/exit\"\n",
        workdir = sh_quote(&agent.workdir),
        cmd = headless_command(agent, resume),
    );
    client
        .upload_bytes(&agent.machine, &format!("{dir}/run.sh"), run.into_bytes())
        .await?;
    // The run inherits fd 9, so it holds the lock until it exits and a second
    // prompt cannot start the same session twice. `offset` marks where this
    // run's output starts, so `read` never reports an earlier run's result.
    let launched = client
        .exec(
            &agent.machine,
            &format!(
                "d={dir}; exec 9>>\"$d/lock\"; \
                 if command -v flock >/dev/null; then flock -n 9 || exit {BUSY_EXIT}; fi; \
                 rm -f \"$d/exit\" \"$d/pid\" \"$d/result.md\"; \
                 if [ -f \"$d/out.jsonl\" ]; then wc -c < \"$d/out.jsonl\"; else echo 0; fi > \"$d/offset\"; \
                 setsid nohup sh \"$d/run.sh\" \"$d\" >/dev/null 2>&1 </dev/null & \
                 for i in 1 2 3 4 5 6 7 8 9 10; do [ -s \"$d/pid\" ] && break; sleep 0.2; done; \
                 [ -s \"$d/pid\" ]"
            ),
        )
        .await?;
    if launched.code == BUSY_EXIT {
        bail!(Coded::new(
            "busy",
            format!(
                "{} is still working; `agent wait {}` first",
                agent.name, agent.name
            ),
        ));
    }
    launched.check("launching the agent")?;
    Ok(())
}

pub struct Status {
    pub status: String,
    pub detail: Option<Value>,
}

impl Status {
    fn new(status: &str) -> Self {
        Self {
            status: status.to_owned(),
            detail: None,
        }
    }
}

fn view(agent: &Agent, status: &Status) -> Value {
    let mut value = json!({
        "agent": agent.name,
        "machine": agent.machine,
        "kind": agent.kind,
        "mode": agent.mode,
        "status": status.status,
        "workdir": agent.workdir,
    });
    if let Some(workspace) = &agent.workspace {
        value["workspace"] = json!(workspace);
    }
    if let Some(session) = &agent.session {
        value["session"] = json!(session);
    }
    if let Some(detail) = &status.detail {
        value["detail"] = detail.clone();
    }
    value
}

fn code_of(err: &anyhow::Error) -> Option<&str> {
    err.chain()
        .find_map(|e| e.downcast_ref::<Coded>())
        .map(|c| c.code.as_ref())
}

pub fn find(name: &str) -> Result<Agent> {
    store::agent(&store::dirs(), name).ok_or_else(|| {
        Coded::new(
            "not_found",
            format!("no agent named {name}; `herdr-boxd agent list` shows them"),
        )
        .into()
    })
}

/// Status without waking sleeping machines.
pub async fn status(client: &mut boxd::Client, agent: &Agent) -> Result<Value> {
    let all = statuses(client, std::slice::from_ref(agent)).await?;
    Ok(all.into_iter().next().unwrap_or(Value::Null))
}

pub async fn list(client: &mut boxd::Client) -> Result<Vec<Value>> {
    let agents = store::agents(&store::dirs());
    statuses(client, &agents).await
}

/// Ledger agents, plus Herdr agents running on plugin machines that were not spawned here.
pub async fn visible(client: &mut boxd::Client) -> Result<Vec<Value>> {
    let mut listed = list(client).await?;
    let mut seen: HashSet<(String, String)> = listed
        .iter()
        .filter_map(|value| {
            Some((
                value["machine"].as_str()?.to_owned(),
                value["agent"].as_str()?.to_owned(),
            ))
        })
        .collect();
    let running: HashSet<String> = client
        .list_vms()
        .await?
        .vms
        .into_iter()
        .filter(|vm| vm.status == "running")
        .map(|vm| vm.name)
        .collect();
    let profiles = herdr::profiles().unwrap_or_default();
    let mut queried = HashSet::new();
    for name in store::machines(&store::dirs()) {
        if !running.contains(&name) {
            continue;
        }
        let Some(profile) = profiles.iter().find(|profile| profile.is_for(&name)) else {
            continue;
        };
        queried.insert(name.clone());
        if let Ok(live) = herdr_agents(Some(&profile.id)).await {
            push_live(&mut listed, &mut seen, &name, &live);
        }
    }
    if let Some(here) = current_boxd_name() {
        if !queried.contains(&here) {
            if let Ok(live) = herdr_agents(None).await {
                push_live(&mut listed, &mut seen, &here, &live);
            }
        }
    }
    Ok(listed)
}

fn current_boxd_name() -> Option<String> {
    std::env::var("boxd_vm_name")
        .ok()
        .filter(|name| !name.is_empty())
        .or_else(|| {
            std::env::var("BOXD_VM_NAME")
                .ok()
                .filter(|name| !name.is_empty())
        })
}

fn push_live(
    listed: &mut Vec<Value>,
    seen: &mut HashSet<(String, String)>,
    machine: &str,
    live: &[Value],
) {
    for agent in live {
        let Some(value) = live_view(machine, agent) else {
            continue;
        };
        let key = (
            value["machine"].as_str().unwrap_or_default().to_owned(),
            value["agent"].as_str().unwrap_or_default().to_owned(),
        );
        if key.1.is_empty() || !seen.insert(key) {
            continue;
        }
        listed.push(value);
    }
}

fn live_view(machine: &str, agent: &Value) -> Option<Value> {
    let kind = agent["agent"].as_str().unwrap_or("");
    let name = herdr_agent_name(agent)?;
    Some(json!({
        "agent": name,
        "machine": machine,
        "kind": if kind.is_empty() { name } else { kind },
        "mode": "pane",
        "status": agent["agent_status"].as_str().unwrap_or("unknown"),
        "workdir": agent["cwd"].as_str().unwrap_or(""),
        "spawned": false,
    }))
}

fn herdr_agent_name(agent: &Value) -> Option<&str> {
    agent["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .or_else(|| agent["agent"].as_str().filter(|name| !name.is_empty()))
}

async fn herdr_agents(profile: Option<&str>) -> Result<Vec<Value>> {
    let value = match profile {
        Some(profile) => {
            herdr::remote_timeout(profile, &["agent", "list"], Duration::from_secs(20)).await?
        }
        None => local_herdr_agents().await?,
    };
    Ok(value["result"]["agents"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

async fn local_herdr_agents() -> Result<Value> {
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new(herdr::bin())
            .args(["agent", "list"])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| Coded::new("timeout", "herdr agent list timed out"))??;
    if !output.status.success() {
        bail!(Coded::new(
            "herdr_failed",
            format!(
                "herdr agent list exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    serde_json::from_slice(&output.stdout).context("herdr agent list is not JSON")
}

// shortcut: machines are queried one after another; parallelise if people run
// many machines at once.
async fn statuses(client: &mut boxd::Client, agents: &[Agent]) -> Result<Vec<Value>> {
    let vms: HashMap<String, String> = client
        .list_vms()
        .await?
        .vms
        .into_iter()
        .map(|vm| (vm.name, vm.status))
        .collect();
    let profiles = herdr::profiles().unwrap_or_default();
    let mut by_machine: HashMap<&str, HashMap<String, Status>> = HashMap::new();
    for machine in agents.iter().map(|a| a.machine.as_str()) {
        if by_machine.contains_key(machine) {
            continue;
        }
        let here: Vec<&Agent> = agents.iter().filter(|a| a.machine == machine).collect();
        let mut found = HashMap::new();
        match vms.get(machine).map(String::as_str) {
            None => {
                for agent in &here {
                    found.insert(agent.name.clone(), Status::new("machine_gone"));
                }
            }
            Some("running") => {
                if here.iter().any(|a| a.mode == Mode::Pane) {
                    let profile = profiles.iter().find(|p| p.is_for(machine));
                    let live = match profile {
                        Some(p) => pane_statuses(&p.id).await,
                        None => Err(Coded::new("not_in_herdr", "no Herdr profile").into()),
                    };
                    for agent in here.iter().filter(|a| a.mode == Mode::Pane) {
                        let status = match &live {
                            Ok(live) => live
                                .get(&agent.name)
                                .map_or_else(|| Status::new("exited"), |s| Status::new(s)),
                            Err(err) => Status {
                                status: "unreachable".into(),
                                detail: Some(json!(format!("{err:#}"))),
                            },
                        };
                        found.insert(agent.name.clone(), status);
                    }
                }
                let headless: Vec<&Agent> = here
                    .iter()
                    .copied()
                    .filter(|a| a.mode == Mode::Headless)
                    .collect();
                if !headless.is_empty() {
                    found.extend(headless_statuses(client, machine, &headless).await);
                }
            }
            Some(other) => {
                for agent in &here {
                    found.insert(agent.name.clone(), Status::new(&format!("machine_{other}")));
                }
            }
        }
        by_machine.insert(machine, found);
    }
    Ok(agents
        .iter()
        .map(|agent| {
            let status = by_machine
                .get_mut(agent.machine.as_str())
                .and_then(|found| found.remove(&agent.name))
                .unwrap_or_else(|| Status::new("unknown"));
            view(agent, &status)
        })
        .collect())
}

async fn pane_statuses(profile: &str) -> Result<HashMap<String, String>> {
    let reply = herdr::remote(profile, &["agent", "list"]).await?;
    Ok(reply["result"]["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some((
                a["name"].as_str()?.to_owned(),
                a["agent_status"].as_str().unwrap_or("unknown").to_owned(),
            ))
        })
        .collect())
}

async fn headless_statuses(
    client: &mut boxd::Client,
    machine: &str,
    agents: &[&Agent],
) -> HashMap<String, Status> {
    let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
    // Agent names are validated to [a-z0-9_-], safe to splice into the script.
    let script = format!(
        r#"{ALIVE}
for n in {names}; do d=/home/boxd/.herdr-boxd/agents/$n
if [ -f "$d/exit" ]; then echo "$n exit $(cat "$d/exit")"
elif alive "$d"; then echo "$n running"
else echo "$n lost"; fi; done"#,
        names = names.join(" ")
    );
    let (text, error) = match client.exec(machine, &script).await {
        Ok(output) => (output.stdout_text(), None),
        Err(err) => (String::new(), Some(format!("{err:#}"))),
    };
    let mut found: HashMap<String, Status> = text
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?.to_owned();
            let status = match (parts.next()?, parts.next()) {
                ("exit", Some("0")) => Status::new("done"),
                ("exit", Some(code)) => Status {
                    status: "failed".into(),
                    detail: Some(json!({ "exit_code": code.parse::<i64>().unwrap_or(-1) })),
                },
                ("running", _) => Status::new("working"),
                _ => Status::new("lost"),
            };
            Some((name, status))
        })
        .collect();
    for name in names {
        found.entry(name.to_owned()).or_insert_with(|| Status {
            status: "unreachable".into(),
            detail: error.clone().map(Value::from),
        });
    }
    found
}

/// Wakes the agent's machine and returns the Herdr profile for pane calls.
async fn ready(client: &mut boxd::Client, agent: &Agent) -> Result<String> {
    match agent.mode {
        Mode::Pane => machine::attach(client, &agent.machine).await,
        Mode::Headless => {
            client.ensure_running(&agent.machine).await?;
            Ok(String::new())
        }
    }
}

pub async fn wait(
    client: &mut boxd::Client,
    agent: &Agent,
    timeout_ms: Option<u64>,
) -> Result<Value> {
    let profile = ready(client, agent).await?;
    let status = match agent.mode {
        Mode::Pane => {
            let timeout = timeout_ms.map(|ms| ms.to_string());
            let mut args = vec!["agent", "wait", agent.name.as_str()];
            if let Some(timeout) = &timeout {
                args.extend(["--timeout", timeout]);
            }
            let reply = herdr::remote(&profile, &args).await?;
            Status::new(
                reply["result"]["agent"]["agent_status"]
                    .as_str()
                    .unwrap_or("unknown"),
            )
        }
        Mode::Headless => wait_headless(client, agent, timeout_ms).await?,
    };
    Ok(view(agent, &status))
}

async fn wait_headless(
    client: &mut boxd::Client,
    agent: &Agent,
    timeout_ms: Option<u64>,
) -> Result<Status> {
    let deadline = timeout_ms.map(|ms| tokio::time::Instant::now() + Duration::from_millis(ms));
    let mut misses = 0;
    loop {
        let mut status = headless_statuses(client, &agent.machine, &[agent]).await;
        let status = status
            .remove(&agent.name)
            .unwrap_or_else(|| Status::new("unknown"));
        if status.status == "unreachable" {
            misses += 1;
            // About 30s of failures: stop guessing and say what is wrong.
            if misses >= 10 {
                if client.get_vm(&agent.machine).await?.is_none() {
                    return Ok(Status::new("machine_gone"));
                }
                bail!(Coded::new(
                    "unreachable",
                    format!(
                        "cannot reach {} on {}: {}",
                        agent.name,
                        agent.machine,
                        status.detail.map(|d| d.to_string()).unwrap_or_default()
                    ),
                ));
            }
        } else if status.status != "working" {
            return Ok(status);
        } else {
            misses = 0;
        }
        if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
            bail!(Coded::new(
                "timeout",
                format!("{} is still {}", agent.name, status.status)
            ));
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

pub async fn read(client: &mut boxd::Client, agent: &Agent, lines: u32) -> Result<Value> {
    let profile = ready(client, agent).await?;
    let lines_text = lines.to_string();
    match agent.mode {
        Mode::Pane => {
            let text = herdr::remote_text(
                &profile,
                &[
                    "agent",
                    "read",
                    &agent.name,
                    "--source",
                    "recent-unwrapped",
                    "--lines",
                    &lines_text,
                ],
            )
            .await?;
            Ok(json!({ "agent": agent.name, "mode": agent.mode, "text": text }))
        }
        Mode::Headless => {
            let dir = agent_dir(&agent.name);
            let out = client
                .exec(
                    &agent.machine,
                    &format!(
                        "tail -n {lines} {dir}/out.jsonl 2>/dev/null; echo '\x1e'; \
                         tail -n 20 {dir}/err.log 2>/dev/null; echo '\x1e'; \
                         cat {dir}/result.md 2>/dev/null; \
                         off=$(cat {dir}/offset 2>/dev/null || echo 0); \
                         tail -c +$((off + 1)) {dir}/out.jsonl 2>/dev/null | grep '\"type\":\"result\"' | tail -n 1"
                    ),
                )
                .await?
                .stdout_text();
            let mut parts = out.splitn(3, "\x1e\n");
            let events: Vec<Value> = parts
                .next()
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap_or_else(|_| json!(line)))
                .collect();
            let stderr = parts.next().unwrap_or_default().trim().to_owned();
            let result = final_result(parts.next().unwrap_or_default());
            let text: String = events
                .iter()
                .filter_map(summarize)
                .map(|line| format!("{line}\n"))
                .collect();
            Ok(json!({
                "agent": agent.name,
                "mode": agent.mode,
                "text": text,
                "result": result,
                "stderr": stderr,
                "events": events,
            }))
        }
    }
}

fn final_result(tail: &str) -> Option<String> {
    let tail = tail.trim();
    if tail.is_empty() {
        return None;
    }
    // A claude `result` event, or else codex's last-message file.
    let last = tail.lines().last().unwrap_or_default();
    if let Ok(event) = serde_json::from_str::<Value>(last) {
        if let Some(result) = event["result"].as_str() {
            return Some(result.to_owned());
        }
    }
    Some(tail.to_owned())
}

/// One readable line per stream-json (claude) or --json (codex) event.
pub fn summarize(event: &Value) -> Option<String> {
    let Some(kind) = event["type"].as_str() else {
        return event.as_str().map(str::to_owned);
    };
    match kind {
        "assistant" => {
            let parts: Vec<String> = event["message"]["content"]
                .as_array()?
                .iter()
                .filter_map(|c| match c["type"].as_str()? {
                    "text" => c["text"].as_str().map(|t| t.trim().to_owned()),
                    "tool_use" => Some(format!("[tool] {}", c["name"].as_str().unwrap_or("?"))),
                    _ => None,
                })
                .filter(|t| !t.is_empty())
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        "result" => Some(format!(
            "[result] {}",
            event["result"].as_str().unwrap_or_default()
        )),
        "item.completed" => {
            let item = &event["item"];
            match item["type"].as_str()? {
                "agent_message" => item["text"].as_str().map(str::to_owned),
                "command_execution" => Some(format!("[cmd] {}", item["command"].as_str()?)),
                _ => None,
            }
        }
        "error" | "turn.failed" => Some(format!("[error] {event}")),
        _ => None,
    }
}

pub async fn prompt(
    client: &mut boxd::Client,
    agent: &mut Agent,
    text: &str,
    wait: bool,
    timeout_ms: Option<u64>,
) -> Result<Value> {
    let profile = ready(client, agent).await?;
    let status = match agent.mode {
        Mode::Pane => pane_prompt(&profile, &agent.name, text, wait, timeout_ms).await?,
        Mode::Headless => {
            let current = headless_statuses(client, &agent.machine, &[agent])
                .await
                .remove(&agent.name)
                .map(|s| s.status)
                .unwrap_or_default();
            if current == "working" || current == "unreachable" {
                bail!(Coded::new(
                    "busy",
                    format!(
                        "{} is still working; `agent wait {}` first",
                        agent.name, agent.name
                    ),
                ));
            }
            if agent.kind == "codex" && agent.session.is_none() {
                agent.session = codex_thread(client, agent).await;
                store::save_agent(&store::dirs(), agent)?;
            }
            run_headless(client, agent, text, true).await?;
            if wait {
                wait_headless(client, agent, timeout_ms).await?
            } else {
                Status::new("working")
            }
        }
    };
    Ok(view(agent, &status))
}

async fn codex_thread(client: &mut boxd::Client, agent: &Agent) -> Option<String> {
    let out = client
        .exec(
            &agent.machine,
            &format!(
                "grep -h -m1 -o '\"thread_id\":\"[^\"]*\"' {}/out.jsonl",
                agent_dir(&agent.name)
            ),
        )
        .await
        .ok()?
        .stdout_text();
    let id = out.trim().rsplit(':').next()?.trim_matches('"').to_owned();
    (!id.is_empty()).then_some(id)
}

pub async fn keys(client: &mut boxd::Client, agent: &Agent, keys: &[String]) -> Result<()> {
    if agent.mode != Mode::Pane {
        bail!(Coded::new("usage", "keys only works for pane agents"));
    }
    let profile = ready(client, agent).await?;
    let mut args = vec!["agent", "send-keys", agent.name.as_str()];
    args.extend(keys.iter().map(String::as_str));
    herdr::remote(&profile, &args).await?;
    Ok(())
}

/// Stops the agent and forgets it. With `destroy`, also destroys the machine
/// if this agent created it and no other agent still uses it.
pub async fn stop(client: &mut boxd::Client, agent: &Agent, destroy: bool) -> Result<Value> {
    let dirs = store::dirs();
    let others = store::agents(&dirs)
        .iter()
        .any(|other| other.machine == agent.machine && other.name != agent.name);
    let destroying = destroy && agent.owns_machine && !others;
    // A sleeping machine resumes the agent when it wakes, so wake it and stop
    // it now rather than forget an agent that would come back unwatched.
    // Destroying the machine stops it anyway.
    if !destroying && client.get_vm(&agent.machine).await?.is_some() {
        let profile = ready(client, agent).await?;
        halt(client, agent, &profile).await.with_context(|| {
            format!(
                "could not stop {}; nothing was forgotten, retry",
                agent.name
            )
        })?;
    }
    store::forget_agent(&dirs, &agent.name)?;
    let destroyed = if destroying {
        machine::destroy(client, &agent.machine).await?;
        true
    } else {
        if destroy {
            ui::warn(format!(
                "kept {}: {}",
                agent.machine,
                if others {
                    "other agents still use it"
                } else {
                    "this agent did not create it"
                }
            ));
        }
        false
    };
    Ok(
        json!({ "agent": agent.name, "stopped": true, "machine": agent.machine, "machine_destroyed": destroyed }),
    )
}

async fn halt(client: &mut boxd::Client, agent: &Agent, profile: &str) -> Result<()> {
    match agent.mode {
        Mode::Pane => {
            let Some(workspace) = &agent.workspace else {
                return Ok(());
            };
            match herdr::remote(profile, &["workspace", "close", workspace]).await {
                Err(err) if code_of(&err) != Some("workspace_not_found") => Err(err),
                _ => Ok(()),
            }
        }
        Mode::Headless => {
            let dir = agent_dir(&agent.name);
            client
                .exec(
                    &agent.machine,
                    &format!(
                        "{ALIVE}\nd={dir}; alive \"$d\" || exit 0; p=$(cat \"$d/pid\"); \
                         kill -TERM -\"$p\" 2>/dev/null; sleep 1; \
                         alive \"$d\" && kill -KILL -\"$p\" 2>/dev/null; \
                         [ -f \"$d/exit\" ] || echo 143 > \"$d/exit\"; true"
                    ),
                )
                .await?
                .check("stopping the agent")
                .map(|_| ())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workdir_must_be_absolute_and_expands_home() {
        assert_eq!(normalize_workdir("~/app/").unwrap(), "/home/boxd/app");
        assert_eq!(normalize_workdir("~").unwrap(), "/home/boxd");
        assert_eq!(normalize_workdir("/srv/x").unwrap(), "/srv/x");
        assert!(normalize_workdir("app").is_err());
        assert!(normalize_workdir("~bob/x").is_err());
    }

    #[test]
    fn sh_quote_survives_single_quotes() {
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn live_herdr_agent_uses_kind_when_unnamed() {
        let agent = json!({
            "agent": "yapi",
            "agent_status": "working",
            "cwd": "/home/boxd/herdr-boxd"
        });
        let view = live_view("herdr-box", &agent).unwrap();
        assert_eq!(view["agent"], "yapi");
        assert_eq!(view["status"], "working");
        assert_eq!(view["machine"], "herdr-box");
        assert_eq!(view["spawned"], false);
        assert_eq!(
            herdr_agent_name(&json!({"name": "fix-auth", "agent": "claude"})),
            Some("fix-auth")
        );
    }

    #[test]
    fn agent_names_follow_herdr_rules() {
        assert!(is_agent_name("fix-auth_2"));
        assert!(!is_agent_name("Fix"));
        assert!(!is_agent_name("a b"));
        assert!(!is_agent_name(&"a".repeat(33)));
    }

    #[test]
    fn headless_commands_quote_args_and_pick_session_flags() {
        let mut agent = Agent {
            name: "a".into(),
            machine: "m".into(),
            kind: "claude".into(),
            mode: Mode::Headless,
            workdir: "/home/boxd".into(),
            workspace: None,
            session: Some("sid".into()),
            args: vec!["--model".into(), "x'y".into()],
            owns_machine: true,
            created: 0,
        };
        let first = headless_command(&agent, false);
        assert!(first.contains("--session-id 'sid'"));
        assert!(first.contains(r"'x'\''y'"));
        assert!(headless_command(&agent, true).contains("--resume 'sid'"));
        agent.kind = "codex".into();
        agent.session = None;
        assert!(headless_command(&agent, true).contains("resume --json"));
        assert!(headless_command(&agent, true).contains("--last -"));
    }

    #[test]
    fn summarize_reads_claude_and_codex_events() {
        let claude = json!({"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","name":"Bash"}]}});
        assert_eq!(summarize(&claude).unwrap(), "hi\n[tool] Bash");
        let codex = json!({"type":"item.completed","item":{"type":"agent_message","text":"done"}});
        assert_eq!(summarize(&codex).unwrap(), "done");
        assert_eq!(
            final_result(r#"{"type":"result","result":"PONG"}"#).unwrap(),
            "PONG"
        );
    }
}
