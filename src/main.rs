mod agent;
mod boxd;
mod copy;
mod herdr;
mod machine;
mod menu;
mod ssh;
mod store;
mod ui;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::json;

use machine::{Plan, Size};

const SKILL: &str = include_str!("../skills/herdr-boxd/SKILL.md");

const USAGE: &str = "\
herdr-boxd: boxd machines for Herdr, and subagents inside them

usage: herdr-boxd [--json] <command>

machines
  start [--name N] [--size 1x4|2x8|4x16|8x32] [--cwd PATH] [--copy|--no-copy]
  list                          machines this plugin created, with live status
  attach MACHINE                wake a machine and reconnect it to Herdr
  update [MACHINE] [--yes]      save a machine back to the configured snapshot
  destroy MACHINE [--yes]

agents (see `herdr-boxd --skill`)
  agent spawn [--name A] [--kind claude] [--machine M | --size S] [--cwd PATH]
              [--workdir DIR] [--headless] [--prompt TEXT | --prompt-file F|-]
              [--wait] [--timeout MS] [-- AGENT_ARGS...]
  agent list | status A | wait A [--timeout MS] | read A [--lines N]
  agent prompt A (TEXT | --prompt-file F|-) [--wait] [--timeout MS]
  agent keys A KEY...           pane agents only, e.g. `agent keys a down enter`
  agent stop A [--destroy]

config
  config show
  config set snapshot NAME
  config set api-key [KEY|-]    no KEY: prompt (hidden) or read stdin
  config unset api-key

--json prints results as JSON on stdout and errors as
{\"error\":{\"code\",\"message\"}} on stderr. Exit 1 on errors, 2 on usage.";

/// An error with a stable machine-readable code for `--json` callers.
#[derive(Debug)]
pub struct Coded {
    pub code: Cow<'static, str>,
    pub message: String,
}

impl Coded {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code: Cow::Borrowed(code),
            message: message.into(),
        }
    }

    pub fn owned(code: String, message: String) -> Self {
        Self {
            code: Cow::Owned(code),
            message,
        }
    }
}

impl std::fmt::Display for Coded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Coded {}

fn usage(message: impl Into<String>) -> anyhow::Error {
    Coded::new("usage", message).into()
}

#[tokio::main]
async fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let split = args.iter().position(|a| a == "--").unwrap_or(args.len());
    if let Some(i) = args[..split].iter().position(|a| a == "--json") {
        args.remove(i);
        ui::set_json(true);
    }
    let code = match run(args).await {
        Ok(()) => 0,
        Err(err) => report(&err),
    };
    std::process::exit(code);
}

fn report(err: &anyhow::Error) -> i32 {
    let code = err
        .chain()
        .find_map(|e| e.downcast_ref::<Coded>())
        .map_or("error", |c| c.code.as_ref());
    if ui::json() {
        eprintln!(
            "{}",
            json!({ "error": { "code": code, "message": format!("{err:#}") } })
        );
    } else {
        ui::fail(format!("{err:#}"));
        if code == "usage" {
            ui::note("run `herdr-boxd --help` for usage");
        }
    }
    if code == "usage" {
        2
    } else {
        1
    }
}

async fn run(raw: Vec<String>) -> Result<()> {
    let mut raw: VecDeque<String> = raw.into();
    let Some(command) = raw.pop_front() else {
        ui::out(USAGE);
        return Ok(());
    };
    let raw: Vec<String> = raw.into();
    match command.as_str() {
        "-h" | "--help" | "help" => {
            ui::out(USAGE);
            Ok(())
        }
        "-V" | "--version" | "version" => {
            ui::out(format!("herdr-boxd {}", env!("CARGO_PKG_VERSION")));
            Ok(())
        }
        "--skill" | "skill" => {
            ui::out(SKILL.trim_end());
            Ok(())
        }
        "open-menu" => open_menu(raw.first().map(String::as_str)),
        "menu" => menu::run(raw.first().map(String::as_str)).await,
        "config" => config_cmd(raw),
        "start" => start_cmd(raw).await,
        "attach" => attach_cmd(raw).await,
        "list" | "ls" => list_cmd(raw).await,
        "update" | "save" => update_cmd(raw).await,
        "destroy" | "rm" => destroy_cmd(raw).await,
        "agent" | "agents" => agent_cmd(raw).await,
        other => Err(usage(format!("unknown command {other:?}"))),
    }
}

/// Parsed `--flag value` / `--flag=value` options, positionals, and anything
/// after `--`.
struct Args {
    pos: VecDeque<String>,
    opts: HashMap<&'static str, String>,
    flags: HashSet<&'static str>,
    rest: Vec<String>,
}

impl Args {
    fn parse(raw: Vec<String>, valued: &[&'static str], flags: &[&'static str]) -> Result<Self> {
        let mut args = Self {
            pos: VecDeque::new(),
            opts: HashMap::new(),
            flags: HashSet::new(),
            rest: Vec::new(),
        };
        let mut iter = raw.into_iter();
        while let Some(arg) = iter.next() {
            if arg == "--" {
                args.rest.extend(iter);
                break;
            }
            let Some(body) = arg.strip_prefix("--") else {
                args.pos.push_back(arg);
                continue;
            };
            let (key, inline) = match body.split_once('=') {
                Some((key, value)) => (key, Some(value.to_owned())),
                None => (body, None),
            };
            if let Some(key) = valued.iter().copied().find(|k| *k == key) {
                let value = match inline {
                    Some(value) => value,
                    None => iter
                        .next()
                        .ok_or_else(|| usage(format!("--{key} needs a value")))?,
                };
                args.opts.insert(key, value);
            } else if let Some(key) = flags.iter().copied().find(|k| *k == key) {
                if inline.is_some() {
                    return Err(usage(format!("--{key} takes no value")));
                }
                args.flags.insert(key);
            } else {
                return Err(usage(format!("unknown option --{key}")));
            }
        }
        Ok(args)
    }

    fn opt(&self, key: &str) -> Option<&str> {
        self.opts.get(key).map(String::as_str)
    }

    fn flag(&self, key: &str) -> bool {
        self.flags.contains(key)
    }

    fn timeout_ms(&self) -> Result<Option<u64>> {
        self.opt("timeout")
            .map(|raw| {
                raw.parse()
                    .map_err(|_| usage(format!("--timeout must be milliseconds, not {raw}")))
            })
            .transpose()
    }

    fn one(&mut self, what: &str) -> Result<String> {
        let value = self
            .pos
            .pop_front()
            .ok_or_else(|| usage(format!("missing {what}")))?;
        if let Some(extra) = self.pos.front() {
            return Err(usage(format!("unexpected argument {extra:?}")));
        }
        Ok(value)
    }
}

fn open_menu(screen: Option<&str>) -> Result<()> {
    let plugin = std::env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| store::PLUGIN_ID.into());
    let mut cmd = Command::new(herdr::bin());
    cmd.args([
        "plugin",
        "pane",
        "open",
        "--plugin",
        &plugin,
        "--entrypoint",
        "menu",
    ]);
    if let Some(screen) = screen {
        cmd.args(["--env", &format!("BOXD_SCREEN={screen}")]);
    }
    if let Some(cwd) = store::context_cwd() {
        cmd.args(["--env", &format!("BOXD_CWD={}", cwd.display())]);
    }
    let status = cmd.status().context("open boxd menu")?;
    if !status.success() {
        bail!("open boxd menu exited {status}");
    }
    Ok(())
}

fn config_cmd(raw: Vec<String>) -> Result<()> {
    let mut args = Args::parse(raw, &[], &[])?;
    let dirs = store::dirs();
    match args.pos.pop_front().as_deref() {
        Some("show") | None => {
            let snapshot = store::snapshot_name(&dirs);
            let api_key = store::api_key(&dirs).is_some();
            if ui::json() {
                ui::print_json(&json!({
                    "snapshot": snapshot,
                    "auth": if api_key { "api_key" } else { "boxd_login" },
                    "config_dir": dirs.config,
                    "state_dir": dirs.state,
                }));
            } else {
                ui::out(format!("snapshot  {snapshot}"));
                ui::out(format!(
                    "auth      {}",
                    if api_key {
                        "API key (stored, mode 600)"
                    } else {
                        "boxd login (`boxd auth login`)"
                    }
                ));
                ui::out(format!("config    {}", dirs.config.display()));
            }
            Ok(())
        }
        Some("set") => match args.pos.pop_front().as_deref() {
            Some("snapshot") => {
                let name = args.one("snapshot name")?;
                store::set_snapshot(&dirs, &name).map_err(|e| usage(e.to_string()))?;
                ui::done(format!("snapshot set to {name}"));
                Ok(())
            }
            Some("api-key") => {
                let key = match args.pos.pop_front() {
                    Some(key) if key != "-" => {
                        ui::warn("the key is now in your shell history; prefer `config set api-key` with no value");
                        key
                    }
                    _ if ui::interactive() => ui::ask_secret("boxd API key:")?,
                    _ => {
                        let mut text = String::new();
                        std::io::stdin()
                            .read_to_string(&mut text)
                            .context("read API key")?;
                        text.trim().to_owned()
                    }
                };
                store::set_api_key(&dirs, Some(&key)).map_err(|e| usage(e.to_string()))?;
                ui::done("API key saved (mode 600)");
                Ok(())
            }
            _ => Err(usage(
                "usage: config set snapshot NAME | config set api-key [KEY|-]",
            )),
        },
        Some("unset") => match args.pos.pop_front().as_deref() {
            Some("api-key") => {
                store::set_api_key(&dirs, None)?;
                ui::done("API key removed; using `boxd auth login`");
                Ok(())
            }
            _ => Err(usage("usage: config unset api-key")),
        },
        Some(other) => Err(usage(format!("unknown config command {other:?}"))),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CopyChoice {
    Ask,
    Yes,
    No,
}

async fn start_cmd(raw: Vec<String>) -> Result<()> {
    let args = Args::parse(
        raw,
        &["name", "size", "cwd", "snapshot"],
        &["copy", "no-copy"],
    )?;
    let size = args.opt("size").map(machine::parse_size).transpose()?;
    let copy = match (args.flag("copy"), args.flag("no-copy")) {
        (true, true) => return Err(usage("pass --copy or --no-copy, not both")),
        (true, false) => CopyChoice::Yes,
        (false, true) => CopyChoice::No,
        (false, false) => CopyChoice::Ask,
    };
    if let Some(extra) = args.pos.front() {
        return Err(usage(format!("unexpected argument {extra:?}")));
    }
    let request = StartRequest {
        name: args.opt("name").map(str::to_owned),
        size,
        cwd: args
            .opt("cwd")
            .map(PathBuf::from)
            .or_else(store::context_cwd),
        copy,
        snapshot: args.opt("snapshot").map(str::to_owned),
    };
    start_flow(request).await
}

pub struct StartRequest {
    pub name: Option<String>,
    pub size: Option<Size>,
    pub cwd: Option<PathBuf>,
    pub copy: CopyChoice,
    pub snapshot: Option<String>,
}

/// Asks for whatever is missing (only in a terminal), packs the project, then
/// creates the machine. Nothing paid happens until every question is answered.
pub async fn start_flow(request: StartRequest) -> Result<()> {
    let interactive = ui::interactive();
    let snapshot = request
        .snapshot
        .unwrap_or_else(|| store::snapshot_name(&store::dirs()));
    let name = match request.name {
        Some(name) => {
            machine::check_machine_name(&name)?;
            Some(name)
        }
        None if interactive => ask_machine_name()?,
        None => None,
    };
    let size = match request.size {
        Some(size) => size,
        None if interactive => menu::ask_size()?,
        None => machine::DEFAULT_SIZE,
    };
    let copy_dir = request
        .cwd
        .filter(|cwd| match store::copy_destination(cwd) {
            Ok(_) => true,
            Err(reason) => {
                if request.copy == CopyChoice::Yes {
                    ui::warn(format!("not copying: {reason}"));
                }
                false
            }
        });
    let copy_dir = match (copy_dir, request.copy) {
        (Some(dir), CopyChoice::Yes) => Some(dir),
        (Some(dir), CopyChoice::Ask) if interactive => {
            let dest = store::copy_destination(&dir).unwrap_or_default();
            ui::confirm(
                &format!("Copy {} to /home/boxd/{dest}?", dir.display()),
                true,
            )?
            .then_some(dir)
        }
        (Some(_), CopyChoice::Ask) => {
            ui::note("not copying the project (pass --copy to copy it)");
            None
        }
        _ => None,
    };
    let packed = copy_dir.as_deref().map(copy::pack).transpose()?;
    if let Some(packed) = &packed {
        if packed.bytes > copy::CONFIRM_BYTES {
            let question = format!(
                "{} is {}. Upload it anyway?",
                packed.dest,
                copy::describe(packed)
            );
            if interactive && !ui::confirm(&question, false)? {
                bail!(Coded::new("cancelled", "cancelled"));
            }
            if !interactive {
                ui::warn(format!(
                    "uploading {} ({})",
                    packed.dest,
                    copy::describe(packed)
                ));
            }
        }
    }
    if interactive {
        let summary = format!(
            "Start {} ({}) from {snapshot}{}?",
            name.as_deref().unwrap_or("a new machine"),
            size.label,
            packed
                .as_ref()
                .map(|p| format!(", copying {} ({})", p.dest, copy::describe(p)))
                .unwrap_or_default()
        );
        if !ui::confirm(&summary, true)? {
            bail!(Coded::new("cancelled", "cancelled"));
        }
    }
    let mut client = machine::connect().await?;
    let plan = Plan {
        name,
        size,
        snapshot,
        packed,
    };
    let ready = machine::start(&mut client, &plan).await?;
    if ui::json() {
        ui::print_json(&serde_json::to_value(&ready)?);
    } else {
        ui::done(format!(
            "{} is ready ({}).{}",
            ui::bold(&ready.machine),
            ready.size,
            if ready.profile.is_some() {
                " It is in the Herdr sidebar."
            } else {
                ""
            }
        ));
        ui::note(format!("shell:  ssh {}", ssh::alias(&ready.machine)));
        if ready.profile.is_some() {
            ui::note(format!(
                "herdr:  herdr --machine {} workspace list",
                ready.machine
            ));
        }
        if ready.workdir != "/home/boxd" {
            ui::note(format!("code:   {}", ready.workdir));
        }
    }
    Ok(())
}

fn ask_machine_name() -> Result<Option<String>> {
    loop {
        let name = ui::ask("Machine name (blank lets boxd pick):")?;
        if name.is_empty() {
            return Ok(None);
        }
        if machine::is_machine_name(&name) {
            return Ok(Some(name));
        }
        ui::warn("use lowercase letters, digits, and '-', starting with a letter");
    }
}

async fn attach_cmd(raw: Vec<String>) -> Result<()> {
    let mut args = Args::parse(raw, &[], &[])?;
    let name = args.one("machine name")?;
    machine::require_owned(&name)?;
    let mut client = machine::connect().await?;
    let profile = machine::attach(&mut client, &name).await?;
    if ui::json() {
        ui::print_json(&json!({ "machine": name, "profile": profile }));
    } else {
        ui::done(format!("{name} is running and connected to Herdr"));
    }
    Ok(())
}

async fn list_cmd(raw: Vec<String>) -> Result<()> {
    Args::parse(raw, &[], &[])?;
    let mut client = machine::connect().await?;
    let machines = machine::list(&mut client).await?;
    if ui::json() {
        ui::print_json(&serde_json::to_value(&machines)?);
        return Ok(());
    }
    if machines.is_empty() {
        ui::out("no machines yet; start one with `herdr-boxd start`");
        return Ok(());
    }
    for line in menu::machine_lines(&machines) {
        ui::out(line);
    }
    Ok(())
}

async fn update_cmd(raw: Vec<String>) -> Result<()> {
    let mut args = Args::parse(raw, &[], &["yes"])?;
    let dirs = store::dirs();
    let name = match args.pos.pop_front() {
        Some(name) => name,
        None => match store::machines(&dirs).as_slice() {
            [only] => only.clone(),
            [] => return Err(Coded::new("not_found", "no plugin machines to save").into()),
            _ => return Err(usage("several machines exist; name the one to save")),
        },
    };
    machine::require_owned(&name)?;
    let snapshot = store::snapshot_name(&dirs);
    confirm_or_yes(
        args.flag("yes"),
        &format!("Overwrite snapshot {snapshot} with {name}? New sessions will start from it."),
    )?;
    let mut client = machine::connect().await?;
    let version = machine::save_snapshot(&mut client, &name, &snapshot).await?;
    if ui::json() {
        ui::print_json(&json!({ "machine": name, "snapshot": snapshot, "version": version }));
    } else {
        ui::done(format!("saved {name} as {snapshot} v{version}"));
    }
    Ok(())
}

async fn destroy_cmd(raw: Vec<String>) -> Result<()> {
    let mut args = Args::parse(raw, &[], &["yes"])?;
    let name = args.one("machine name")?;
    machine::require_owned(&name)?;
    confirm_or_yes(
        args.flag("yes"),
        &format!("Destroy {name}? This deletes the machine and its disk."),
    )?;
    let mut client = machine::connect().await?;
    let existed = machine::destroy(&mut client, &name).await?;
    if ui::json() {
        ui::print_json(&json!({ "machine": name, "destroyed": existed }));
    } else if existed {
        ui::done(format!("destroyed {name}"));
    } else {
        ui::done(format!("{name} was already gone; cleaned up local state"));
    }
    Ok(())
}

/// Destructive commands need --yes unless a person can answer the question.
fn confirm_or_yes(yes: bool, question: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !ui::interactive() {
        return Err(usage("this is destructive; pass --yes to confirm"));
    }
    if ui::confirm(question, false)? {
        Ok(())
    } else {
        Err(Coded::new("cancelled", "cancelled").into())
    }
}

fn read_prompt(args: &Args, inline: Option<String>) -> Result<Option<String>> {
    let text = match (
        args.opt("prompt").map(str::to_owned).or(inline),
        args.opt("prompt-file"),
    ) {
        (Some(_), Some(_)) => return Err(usage("pass a prompt or --prompt-file, not both")),
        (Some(text), None) => text,
        (None, Some("-")) => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .context("read prompt from stdin")?;
            text
        }
        (None, Some(path)) => {
            std::fs::read_to_string(path).with_context(|| format!("read prompt file {path}"))?
        }
        (None, None) => return Ok(None),
    };
    if text.trim().is_empty() {
        return Err(usage("the prompt is empty"));
    }
    Ok(Some(text))
}

async fn agent_cmd(raw: Vec<String>) -> Result<()> {
    let mut raw: VecDeque<String> = raw.into();
    let sub = raw.pop_front().unwrap_or_else(|| "list".into());
    let raw: Vec<String> = raw.into();
    let emit = |value: serde_json::Value| {
        if ui::json() {
            ui::print_json(&value);
        } else {
            menu::print_agent(&value);
        }
    };
    match sub.as_str() {
        "spawn" | "new" => {
            let args = Args::parse(
                raw,
                &[
                    "name",
                    "kind",
                    "machine",
                    "size",
                    "cwd",
                    "workdir",
                    "prompt",
                    "prompt-file",
                    "timeout",
                ],
                &["headless", "wait"],
            )?;
            if let Some(extra) = args.pos.front() {
                return Err(usage(format!(
                    "unexpected argument {extra:?}; pass the task with --prompt"
                )));
            }
            if args.opt("machine").is_some() && args.opt("size").is_some() {
                return Err(usage(
                    "--size only applies to a new machine, not with --machine",
                ));
            }
            let opts = agent::SpawnOpts {
                name: args.opt("name").map(str::to_owned),
                kind: args.opt("kind").unwrap_or("claude").to_owned(),
                machine: args.opt("machine").map(str::to_owned),
                size: args
                    .opt("size")
                    .map(machine::parse_size)
                    .transpose()?
                    .unwrap_or(machine::DEFAULT_SIZE),
                cwd: args.opt("cwd").map(PathBuf::from),
                workdir: args.opt("workdir").map(str::to_owned),
                headless: args.flag("headless"),
                prompt: read_prompt(&args, None)?,
                wait: args.flag("wait"),
                timeout_ms: args.timeout_ms()?,
                args: args.rest.clone(),
            };
            if opts.cwd.is_some() && opts.workdir.is_some() {
                return Err(usage(
                    "--cwd copies a project and picks the workdir; drop --workdir",
                ));
            }
            emit(agent::spawn(opts).await?);
        }
        "list" | "ls" => {
            Args::parse(raw, &[], &[])?;
            let mut client = machine::connect().await?;
            let agents = agent::list(&mut client).await?;
            if ui::json() {
                ui::print_json(&json!(agents));
            } else if agents.is_empty() {
                ui::out("no agents; spawn one with `herdr-boxd agent spawn --prompt ...`");
            } else {
                agents.iter().for_each(menu::print_agent);
            }
        }
        "status" | "get" => {
            let mut args = Args::parse(raw, &[], &[])?;
            let agent = agent::find(&args.one("agent name")?)?;
            let mut client = machine::connect().await?;
            emit(agent::status(&mut client, &agent).await?);
        }
        "wait" => {
            let mut args = Args::parse(raw, &["timeout"], &[])?;
            let agent = agent::find(&args.one("agent name")?)?;
            let mut client = machine::connect().await?;
            emit(agent::wait(&mut client, &agent, args.timeout_ms()?).await?);
        }
        "read" | "logs" => {
            let mut args = Args::parse(raw, &["lines"], &[])?;
            let agent = agent::find(&args.one("agent name")?)?;
            let lines = args
                .opt("lines")
                .map(|raw| raw.parse().map_err(|_| usage("--lines must be a number")))
                .transpose()?
                .unwrap_or(120u32)
                .clamp(1, 10_000);
            let mut client = machine::connect().await?;
            let value = agent::read(&mut client, &agent, lines).await?;
            if ui::json() {
                ui::print_json(&value);
            } else {
                ui::out(value["text"].as_str().unwrap_or_default().trim_end());
                if let Some(result) = value["result"].as_str() {
                    ui::out(format!("\n{}\n{result}", ui::bold("result")));
                }
                if let Some(stderr) = value["stderr"].as_str().filter(|s| !s.is_empty()) {
                    ui::out(format!("\n{}\n{stderr}", ui::bold("stderr")));
                }
            }
        }
        "prompt" | "send" => {
            let mut args = Args::parse(raw, &["prompt-file", "timeout"], &["wait"])?;
            let name = args
                .pos
                .pop_front()
                .ok_or_else(|| usage("missing agent name"))?;
            let inline = args.pos.drain(..).collect::<Vec<_>>().join(" ");
            let text = read_prompt(&args, (!inline.is_empty()).then_some(inline))?
                .ok_or_else(|| usage("missing prompt text"))?;
            let mut agent = agent::find(&name)?;
            let mut client = machine::connect().await?;
            emit(
                agent::prompt(
                    &mut client,
                    &mut agent,
                    &text,
                    args.flag("wait"),
                    args.timeout_ms()?,
                )
                .await?,
            );
        }
        "keys" | "send-keys" => {
            let mut args = Args::parse(raw, &[], &[])?;
            let name = args
                .pos
                .pop_front()
                .ok_or_else(|| usage("missing agent name"))?;
            let keys: Vec<String> = args.pos.drain(..).collect();
            if keys.is_empty() {
                return Err(usage(
                    "name at least one key, e.g. enter, esc, down, ctrl+c",
                ));
            }
            let agent = agent::find(&name)?;
            let mut client = machine::connect().await?;
            agent::keys(&mut client, &agent, &keys).await?;
            if ui::json() {
                ui::print_json(&json!({ "agent": name, "sent": keys }));
            } else {
                ui::done(format!("sent {} to {name}", keys.join(" ")));
            }
        }
        "stop" | "rm" => {
            let mut args = Args::parse(raw, &[], &["destroy"])?;
            let agent = agent::find(&args.one("agent name")?)?;
            let mut client = machine::connect().await?;
            let value = agent::stop(&mut client, &agent, args.flag("destroy")).await?;
            if ui::json() {
                ui::print_json(&value);
            } else {
                ui::done(format!(
                    "stopped {}{}",
                    agent.name,
                    if value["machine_destroyed"] == true {
                        format!(" and destroyed {}", agent.machine)
                    } else {
                        String::new()
                    }
                ));
            }
        }
        other => return Err(usage(format!("unknown agent command {other:?}"))),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn args_parse_values_flags_and_rest() {
        let args = Args::parse(
            strings(&[
                "a",
                "--kind=codex",
                "--wait",
                "--name",
                "x",
                "--",
                "--model",
                "o3",
            ]),
            &["kind", "name"],
            &["wait"],
        )
        .unwrap();
        assert_eq!(args.pos, ["a"]);
        assert_eq!(args.opt("kind"), Some("codex"));
        assert_eq!(args.opt("name"), Some("x"));
        assert!(args.flag("wait"));
        assert_eq!(args.rest, ["--model", "o3"]);
    }

    #[test]
    fn args_reject_unknown_and_missing_values() {
        assert!(Args::parse(strings(&["--nope"]), &[], &[]).is_err());
        assert!(Args::parse(strings(&["--name"]), &["name"], &[]).is_err());
        assert!(Args::parse(strings(&["--wait=1"]), &[], &["wait"]).is_err());
    }
}
