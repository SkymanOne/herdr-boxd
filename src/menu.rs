//! The Herdr popup. Single-key choices, Esc goes back, errors stay on screen
//! until a key is pressed.

use anyhow::{bail, Result};
use serde_json::Value;

use crate::machine::{self, Listed, Size, SIZES};
use crate::ui::{self, Key};
use crate::{agent, store, tree, Coded, CopyChoice, StartRequest};

pub async fn run(screen: Option<&str>) -> Result<()> {
    let screen = screen
        .map(str::to_owned)
        .or_else(|| std::env::var("BOXD_SCREEN").ok())
        .unwrap_or_default();
    let result = match screen.as_str() {
        "start" => start().await,
        "configure" => configure(),
        "update" => update().await,
        "destroy" => destroy().await,
        "agents" => agents().await,
        _ => return home().await,
    };
    // A direct action: show the outcome before the popup closes.
    finish(result);
    Ok(())
}

fn finish(result: Result<()>) {
    match result {
        Ok(()) => ui::pause(),
        Err(err) if is_cancel(&err) => {}
        Err(err) => {
            ui::fail(format!("{err:#}"));
            ui::pause();
        }
    }
}

fn is_cancel(err: &anyhow::Error) -> bool {
    err.chain()
        .find_map(|e| e.downcast_ref::<Coded>())
        .is_some_and(|c| c.code == "cancelled")
}

async fn home() -> Result<()> {
    loop {
        ui::clear();
        header();
        match fetch_machines().await {
            Ok(machines) if machines.is_empty() => {
                ui::out(ui::dim("  no machines yet; press 1 to start one"))
            }
            Ok(machines) => {
                for line in machine_lines(&machines) {
                    ui::out(format!("  {line}"));
                }
            }
            Err(err) => ui::out(format!("  {}", ui::paint("31", format!("{err:#}")))),
        }
        ui::out("");
        ui::out(format!(
            "  {}  Start session     {}  Snapshot name     {}  Save to snapshot",
            ui::bold("1"),
            ui::bold("2"),
            ui::bold("3")
        ));
        ui::out(format!(
            "  {}  Destroy machine   {}  Agents            {}  Refresh   {}  Quit",
            ui::bold("4"),
            ui::bold("5"),
            ui::bold("r"),
            ui::bold("q")
        ));
        let result = match ui::read_key()? {
            Key::Char('1') => start().await,
            Key::Char('2') => configure(),
            Key::Char('3') => update().await,
            Key::Char('4') => destroy().await,
            Key::Char('5') => agents().await,
            Key::Char('r') => {
                ui::step("syncing Herdr machines");
                if let Ok(mut client) = machine::connect().await {
                    tree::sync_best_effort(&mut client).await;
                }
                continue;
            }
            Key::Char('q') | Key::Cancel => return Ok(()),
            _ => continue,
        };
        finish(result);
    }
}

fn header() {
    let dirs = store::dirs();
    ui::out(format!(
        "{}  {}",
        ui::bold("Boxd"),
        ui::dim(format!("snapshot {}", store::snapshot_name(&dirs)))
    ));
    if let Some(cwd) = store::context_cwd() {
        ui::out(ui::dim(format!("cwd {}", cwd.display())));
    }
    ui::out("");
}

async fn fetch_machines() -> Result<Vec<Listed>> {
    let mut client = machine::connect().await?;
    tree::list(&mut client).await
}

/// Every machine with its depth, parents before children.
fn rows(machines: &[Listed], depth: usize, out: &mut Vec<(usize, Listed)>) {
    for m in machines {
        out.push((depth, m.clone()));
        rows(&m.children, depth + 1, out);
    }
}

/// One line per machine; the machines each one created are indented below it.
pub fn machine_lines(machines: &[Listed]) -> Vec<String> {
    let herdr = crate::herdr::available();
    let profiles = if herdr {
        crate::herdr::profiles().unwrap_or_default()
    } else {
        Vec::new()
    };
    let in_herdr_here = |name: &str| profiles.iter().any(|p| p.is_for(name));
    let mut all = Vec::new();
    rows(machines, 0, &mut all);
    let label = |depth: usize, name: &str| match depth {
        0 => name.to_owned(),
        _ => format!("{}└ {name}", "  ".repeat(depth - 1)),
    };
    let width = all
        .iter()
        .map(|(depth, m)| label(*depth, &m.name).chars().count())
        .max()
        .unwrap_or(0);
    all.iter()
        .map(|(depth, m)| {
            let dot = match m.status.as_str() {
                "running" => ui::paint("32", "●"),
                "gone" => ui::paint("31", "●"),
                _ => ui::paint("33", "●"),
            };
            let mut extra = Vec::new();
            if let Some(size) = &m.size {
                extra.push(size.clone());
            }
            if m.agents > 0 {
                extra.push(format!(
                    "{} agent{}",
                    m.agents,
                    if m.agents == 1 { "" } else { "s" }
                ));
            }
            if let Some(reason) = &m.unreachable {
                extra.push(format!("its machines are unknown: {reason}"));
            }
            if m.status == "gone" {
                extra.push("destroyed outside the plugin; destroy to clean up".into());
            } else if *depth == 0 && herdr && !m.in_herdr {
                extra.push("not in Herdr; `herdr-boxd attach` fixes it".into());
            } else if *depth > 0 && herdr && m.status == "running" && !in_herdr_here(&m.name) {
                extra.push("not in Herdr here; `herdr-boxd sync` adds it".into());
            }
            let name = label(*depth, &m.name);
            let pad = width.saturating_sub(name.chars().count());
            format!(
                "{dot} {name}{}  {:11} {}",
                " ".repeat(pad),
                m.status,
                ui::dim(extra.join(" · "))
            )
        })
        .collect()
}

pub fn ask_size() -> Result<Size> {
    let choices: Vec<String> = SIZES
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let label = format!("{}={} vCPU/{} GB", i + 1, s.vcpu, s.memory_bytes >> 30);
            if *s == machine::DEFAULT_SIZE {
                ui::bold(label)
            } else {
                label
            }
        })
        .collect();
    eprintln!("{} {}", ui::bold("Size:"), choices.join("  "));
    eprint!("{} ", ui::dim("[Enter = 2 vCPU/8 GB]"));
    let size = loop {
        match ui::read_key()? {
            Key::Enter => break machine::DEFAULT_SIZE,
            Key::Cancel => bail!(Coded::new("cancelled", "cancelled")),
            Key::Char(c) => {
                if let Some(size) = c
                    .to_digit(10)
                    .and_then(|d| SIZES.get((d as usize).wrapping_sub(1)))
                {
                    break *size;
                }
            }
        }
    };
    eprintln!("{}", size.label);
    Ok(size)
}

async fn start() -> Result<()> {
    ui::clear();
    ui::out(ui::bold("Start a session"));
    ui::out(ui::dim(
        "Esc cancels at any question. Nothing is created until you confirm.",
    ));
    ui::out("");
    crate::start_flow(StartRequest {
        name: None,
        size: None,
        cwd: store::context_cwd(),
        copy: CopyChoice::Ask,
        snapshot: None,
    })
    .await
}

fn configure() -> Result<()> {
    let dirs = store::dirs();
    ui::clear();
    ui::out(ui::bold("Snapshot name"));
    ui::out(ui::dim(format!(
        "New sessions start from this snapshot. Now: {}",
        store::snapshot_name(&dirs)
    )));
    ui::out("");
    let name = ui::ask("New snapshot name (blank keeps it):")?;
    if name.is_empty() {
        bail!(Coded::new("cancelled", "unchanged"));
    }
    store::set_snapshot(&dirs, &name).map_err(|e| Coded::new("usage", e.to_string()))?;
    ui::done(format!("new sessions will start from {name}"));
    Ok(())
}

/// Number keys pick from up to nine machines; Esc cancels.
fn pick(names: &[String], verb: &str) -> Result<String> {
    match names {
        [] => bail!(Coded::new(
            "not_found",
            format!("no plugin machines to {verb}")
        )),
        [only] => return Ok(only.clone()),
        _ => {}
    }
    for (i, name) in names.iter().enumerate().take(9) {
        ui::out(format!("  {}  {name}", ui::bold(i + 1)));
    }
    eprint!("{} ", ui::bold(format!("Which machine to {verb}?")));
    loop {
        match ui::read_key()? {
            Key::Cancel => bail!(Coded::new("cancelled", "cancelled")),
            Key::Char(c) => {
                if let Some(name) = c
                    .to_digit(10)
                    .and_then(|d| names.get((d as usize).wrapping_sub(1)))
                {
                    eprintln!("{name}");
                    return Ok(name.clone());
                }
            }
            Key::Enter => {}
        }
    }
}

async fn update() -> Result<()> {
    let dirs = store::dirs();
    let snapshot = store::snapshot_name(&dirs);
    ui::clear();
    ui::out(ui::bold("Save a machine to the snapshot"));
    ui::out(ui::dim(format!(
        "This replaces {snapshot}; every new session starts from it."
    )));
    ui::out("");
    let name = pick(&store::machines(&dirs), "save")?;
    if !ui::confirm(&format!("Overwrite {snapshot} with {name}?"), false)? {
        bail!(Coded::new("cancelled", "cancelled"));
    }
    let mut client = machine::connect().await?;
    let version = machine::save_snapshot(&mut client, &name, &snapshot).await?;
    ui::done(format!("saved {name} as {snapshot} v{version}"));
    Ok(())
}

async fn destroy() -> Result<()> {
    let dirs = store::dirs();
    ui::clear();
    ui::out(ui::bold("Destroy a machine"));
    ui::out(ui::dim("Only machines this plugin created are listed."));
    ui::out("");
    let name = pick(&store::machines(&dirs), "destroy")?;
    let agents = store::agents(&dirs)
        .iter()
        .filter(|a| a.machine == name)
        .count();
    let warning = if agents > 0 {
        format!(" Its {agents} agent(s) stop too.")
    } else {
        String::new()
    };
    let mut client = machine::connect().await?;
    let below = tree::descendants(&mut client, &name).await?;
    if !ui::confirm(
        &format!("{}{warning}", crate::destroy_question(&name, &below)),
        false,
    )? {
        bail!(Coded::new("cancelled", "cancelled"));
    }
    let existed = machine::destroy_tree(&mut client, &name, &below).await?;
    ui::done(if existed {
        format!("destroyed {name}")
    } else {
        format!("{name} was already gone; cleaned up")
    });
    Ok(())
}

async fn agents() -> Result<()> {
    loop {
        ui::clear();
        ui::out(ui::bold("Agents"));
        ui::out(ui::dim(
            "Herdr agents on these machines. x stops ones started with `herdr-boxd agent spawn`.",
        ));
        ui::out("");
        let mut client = machine::connect().await?;
        // The same tree as `agent list`, plus Herdr agents not spawned here.
        let mut list = agent::visible(&mut client).await?;
        list.extend(tree::remote_agents(&mut client).await?);
        if list.is_empty() {
            ui::out(ui::dim("  no agents"));
        }
        for (i, value) in list.iter().enumerate().take(9) {
            ui::out(format!("  {}  {}", ui::bold(i + 1), agent_line(value)));
        }
        ui::out("");
        ui::out(format!(
            "  {}  Stop an agent   {}  Refresh   {}  Back",
            ui::bold("x"),
            ui::bold("r"),
            ui::bold("Esc")
        ));
        match ui::read_key()? {
            Key::Char('x') if !list.is_empty() => {
                let names: Vec<String> = list
                    .iter()
                    .filter_map(|v| v["agent"].as_str().map(str::to_owned))
                    .collect();
                let name = match pick(&names, "stop") {
                    Ok(name) => name,
                    Err(_) => continue,
                };
                let entry = list
                    .iter()
                    .find(|v| v["agent"] == name.as_str())
                    .cloned()
                    .unwrap_or_default();
                if let Some(host) = entry["host"].as_str() {
                    let machine = entry["machine"].as_str().unwrap_or_default();
                    let destroy = ui::confirm(
                        &format!("Also destroy {machine} (only if {name} created it)?"),
                        false,
                    )?;
                    crate::stop_on(&mut client, host, &entry, destroy, false).await?;
                    ui::done(format!("stopped {name} on {host}"));
                    ui::pause();
                    continue;
                }
                let found = match agent::find(&name) {
                    Ok(found) => found,
                    Err(_) => {
                        ui::warn(format!("{name} was not started by herdr-boxd"));
                        ui::pause();
                        continue;
                    }
                };
                let destroy = found.owns_machine
                    && ui::confirm(&format!("Also destroy {}?", found.machine), false)?;
                agent::stop(&mut client, &found, destroy, false).await?;
                ui::done(format!("stopped {name}"));
                ui::pause();
            }
            Key::Cancel | Key::Char('q') => return Err(Coded::new("cancelled", "back").into()),
            _ => {}
        }
    }
}

fn agent_line(value: &Value) -> String {
    let status = value["status"].as_str().unwrap_or("?");
    let color = match status {
        "working" => "36",
        "idle" | "done" => "32",
        "blocked" => "33",
        _ => "31",
    };
    let via = value["host"]
        .as_str()
        .map(|host| format!(" · via {host}"))
        .unwrap_or_default();
    format!(
        "{:16} {} {}",
        value["agent"].as_str().unwrap_or("?"),
        ui::paint(color, format!("{status:10}")),
        ui::dim(format!(
            "{} {} on {} · {}{via}",
            value["mode"].as_str().unwrap_or(""),
            value["kind"].as_str().unwrap_or(""),
            value["machine"].as_str().unwrap_or(""),
            value["workdir"].as_str().unwrap_or("")
        ))
    )
}

pub fn print_agent(value: &Value) {
    ui::out(agent_line(value));
    if let Some(detail) = value.get("detail") {
        ui::out(format!("  {}", ui::dim(detail)));
    }
}
