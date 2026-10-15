//! The Herdr popup. Single-key choices, Esc goes back, errors stay on screen
//! until a key is pressed.

use anyhow::{bail, Result};
use serde_json::Value;

use crate::machine::{self, Listed, Size, SIZES};
use crate::ui::{self, Key};
use crate::{agent, store, Coded, CopyChoice, StartRequest};

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
    machine::list(&mut client).await
}

pub fn machine_lines(machines: &[Listed]) -> Vec<String> {
    let herdr = crate::herdr::available();
    let width = machines.iter().map(|m| m.name.len()).max().unwrap_or(0);
    machines
        .iter()
        .map(|m| {
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
            if m.status == "gone" {
                extra.push("destroyed outside the plugin; destroy to clean up".into());
            } else if herdr && !m.in_herdr {
                extra.push("not in Herdr; `herdr-boxd attach` fixes it".into());
            }
            format!(
                "{dot} {:width$}  {:11} {}",
                m.name,
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
    if !ui::confirm(
        &format!("Destroy {name}? The machine and its disk are deleted.{warning}"),
        false,
    )? {
        bail!(Coded::new("cancelled", "cancelled"));
    }
    let mut client = machine::connect().await?;
    let existed = machine::destroy(&mut client, &name).await?;
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
            "Spawned with `herdr-boxd agent spawn`. Pane agents also show in the machine's Herdr sidebar.",
        ));
        ui::out("");
        let mut client = machine::connect().await?;
        let list = agent::list(&mut client).await?;
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
                let found = agent::find(&name)?;
                let destroy = found.owns_machine
                    && ui::confirm(&format!("Also destroy {}?", found.machine), false)?;
                agent::stop(&mut client, &found, destroy).await?;
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
    format!(
        "{:16} {} {}",
        value["agent"].as_str().unwrap_or("?"),
        ui::paint(color, format!("{status:10}")),
        ui::dim(format!(
            "{} {} on {} · {}",
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
