mod boxd;
mod ssh;
mod store;

use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("open-menu") => open_menu(args.next().as_deref()),
        Some("menu") => menu(args.next().as_deref()).await,
        Some("config") => config_cmd(&mut args),
        Some("start") => start_cmd(&mut args).await,
        Some("update") => update_cmd(args.next().as_deref()).await,
        Some("destroy") => destroy_cmd(&mut args).await,
        Some("list") => {
            for name in store::machines(&store::dirs()) {
                println!("{name}");
            }
            Ok(())
        }
        _ => {
            eprintln!(
                "usage: herdr-boxd <menu|config|start|update|destroy|list>\n\
                 config set snapshot <name>\n\
                 config set api-key <bxd_...>\n\
                 start [--name NAME] [--size 1x4|2x8|4x16|8x32] [--cwd PATH] [--copy|--no-copy]\n\
                 update [machine]\n\
                 destroy [machine] [--yes]"
            );
            std::process::exit(2);
        }
    }
}

fn open_menu(screen: Option<&str>) -> Result<()> {
    let herdr = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    let plugin = std::env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| store::PLUGIN_ID.into());
    let mut cmd = Command::new(herdr);
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
    if status.success() {
        Ok(())
    } else {
        bail!("open boxd menu exited {status}");
    }
}

async fn menu(screen: Option<&str>) -> Result<()> {
    let screen = screen
        .map(str::to_owned)
        .or_else(|| std::env::var("BOXD_SCREEN").ok())
        .unwrap_or_default();
    match screen.as_str() {
        "start" => start_interactive(None, None, None, CopyChoice::Ask).await,
        "configure" => configure_interactive(),
        "update" => update_interactive().await,
        "destroy" => destroy_interactive().await,
        _ => menu_loop().await,
    }
}

async fn menu_loop() -> Result<()> {
    let dirs = store::dirs();
    loop {
        println!("Boxd  snapshot: {}", store::snapshot_name(&dirs));
        if let Some(cwd) = store::context_cwd() {
            println!("cwd: {}", cwd.display());
        }
        println!("1  Start session");
        println!("2  Configure snapshot");
        println!("3  Update snapshot");
        println!("4  Destroy machine");
        println!("q  Quit");
        print!("> ");
        io::stdout().flush().ok();
        let choice = match read_key() {
            Ok(key) => key,
            Err(err) => {
                show_error(&err);
                continue;
            }
        };
        println!("{choice}");
        let result = match choice {
            '1' => start_interactive(None, None, None, CopyChoice::Ask).await,
            '2' => configure_interactive(),
            '3' => update_interactive().await,
            '4' => destroy_interactive().await,
            'q' | '\u{3}' => return Ok(()),
            _ => {
                println!("press 1, 2, 3, 4, or q");
                continue;
            }
        };
        if let Err(err) = result {
            show_error(&err);
        }
    }
}

fn config_cmd(args: &mut impl Iterator<Item = String>) -> Result<()> {
    let dirs = store::dirs();
    match args.next().as_deref() {
        Some("show") | None => {
            println!("snapshot={}", store::snapshot_name(&dirs));
            println!(
                "api_key={}",
                if store::api_key(&dirs).is_some() {
                    "set"
                } else {
                    "using boxd login"
                }
            );
            Ok(())
        }
        Some("set") => {
            let key = args
                .next()
                .context("usage: config set snapshot|api-key VALUE")?;
            let value = args.next().context("missing value")?;
            match key.as_str() {
                "snapshot" => store::set_snapshot(&dirs, &value).context("save snapshot name"),
                "api-key" => store::set_api_key(&dirs, &value).context("save api key"),
                _ => bail!("usage: config set snapshot|api-key VALUE"),
            }
        }
        _ => bail!("usage: config show | config set snapshot|api-key VALUE"),
    }
}

#[derive(Clone, Copy)]
enum CopyChoice {
    Ask,
    Yes,
    No,
}

async fn start_cmd(args: &mut impl Iterator<Item = String>) -> Result<()> {
    let mut cwd = None;
    let mut name = None;
    let mut size = None;
    let mut copy = CopyChoice::Ask;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cwd" => cwd = Some(PathBuf::from(args.next().context("missing --cwd")?)),
            "--name" => name = Some(args.next().context("missing --name")?),
            "--size" => size = Some(parse_size(&args.next().context("missing --size")?)?),
            "--copy" => copy = CopyChoice::Yes,
            "--no-copy" => copy = CopyChoice::No,
            _ => bail!("usage: start [--name NAME] [--size 2x8] [--cwd PATH] [--copy|--no-copy]"),
        }
    }
    start_interactive(name, size, cwd, copy).await
}

async fn start_interactive(
    name: Option<String>,
    size: Option<MachineSize>,
    cwd: Option<PathBuf>,
    copy: CopyChoice,
) -> Result<()> {
    let dirs = store::dirs();
    let snapshot = store::snapshot_name(&dirs);
    let cwd = cwd.or_else(store::context_cwd);
    let chosen = match name {
        Some(name) => name,
        None if io::stdin().is_terminal() => ask_machine_name()?,
        None => String::new(),
    };
    let size = match size {
        Some(size) => size,
        None if io::stdin().is_terminal() => ask_size()?,
        None => DEFAULT_SIZE,
    };
    println!("creating a machine from {snapshot}");
    let mut client = connect().await?;
    let name = if chosen.is_empty() {
        client.suggest_name().await?
    } else {
        chosen
    };
    let name = client.create_from_snapshot(&snapshot, &name).await?;
    println!(
        "created {name} at {}, waiting until it can resize",
        size.label
    );
    let ready = match client.wait_ssh(&name).await {
        Ok(vm) => vm,
        Err(err) => {
            let _ = client.destroy(&name).await;
            return Err(err);
        }
    };
    if ready.vcpu != size.vcpu || ready.memory_bytes != size.memory_bytes {
        println!("resizing to {} (this reboots)", size.label);
        client.resize(&name, size.vcpu, size.memory_bytes).await?;
    } else {
        println!("rebooting so the shell hostname matches {name}");
        client.reboot(&name).await?;
    }
    println!("waiting for SSH");
    let ready = match client.wait_ssh(&name).await {
        Ok(vm) => vm,
        Err(err) => {
            let _ = client.destroy(&name).await;
            return Err(err);
        }
    };
    let host_key = client.ssh_host_key().await?;
    let identity = ssh::identity_file().context("boxd SSH identity")?;
    let domain = if ready.access_domain.is_empty() {
        "boxd.sh".to_owned()
    } else {
        ready.access_domain
    };
    let target = ssh::SshTarget {
        alias: format!("{name}.boxd"),
        hostname: format!("{name}.{domain}"),
        port: ready.ssh_port,
        identity,
    };
    ssh::install(&target, &host_key).context("write SSH config")?;
    if let Err(err) = herdr_machine_add(&target.alias, &name) {
        let _ = client.destroy(&name).await;
        let _ = ssh::remove(&target.alias);
        return Err(err);
    }
    store::remember(&dirs, &name).context("record machine")?;
    if let Some(cwd) = cwd {
        maybe_copy(&mut client, &name, &cwd, copy).await?;
    }
    println!("connected {name}");
    println!("herdr --machine {name}");
    Ok(())
}

async fn maybe_copy(
    client: &mut boxd::Client,
    vm: &str,
    cwd: &Path,
    copy: CopyChoice,
) -> Result<()> {
    let dest = match store::copy_destination(cwd) {
        Ok(dest) => dest,
        Err(err) => {
            println!("{err}");
            return Ok(());
        }
    };
    let yes = match copy {
        CopyChoice::Yes => true,
        CopyChoice::No => false,
        CopyChoice::Ask => {
            if !io::stdin().is_terminal() {
                println!("not a terminal; skipping copy (pass --copy to force)");
                false
            } else {
                print!("Copy {} to /home/boxd/{dest}? [y/N] ", cwd.display());
                io::stdout().flush().ok();
                matches!(read_key()?, 'y' | 'Y')
            }
        }
    };
    if !yes {
        return Ok(());
    }
    println!("packing {dest} (skipping target/)");
    io::stdout().flush().ok();
    let tar = std::env::temp_dir().join(format!("herdr-boxd-{vm}.tar"));
    let parent = cwd.parent().unwrap_or(Path::new("/"));
    let status = std::process::Command::new("tar")
        .args(["-C"])
        .arg(parent)
        .args(["-cf"])
        .arg(&tar)
        .args(["--exclude", "target"])
        .arg(&dest)
        .status()
        .context("tar")?;
    if !status.success() {
        let _ = std::fs::remove_file(&tar);
        bail!("tar exited {status}");
    }
    let remote = format!("/tmp/herdr-boxd-{vm}.tar");
    let uploaded = client.upload_tar(vm, &remote, &tar).await;
    let _ = std::fs::remove_file(&tar);
    uploaded?;
    client
        .exec(
            vm,
            &format!("mkdir -p /home/boxd && tar -xf {remote} -C /home/boxd && rm -f {remote}"),
        )
        .await?;
    println!("copied to /home/boxd/{dest}");
    Ok(())
}

async fn update_cmd(name: Option<&str>) -> Result<()> {
    let dirs = store::dirs();
    let name = match name {
        Some(name) => name.to_owned(),
        None => pick_machine(&store::machines(&dirs), "update")?,
    };
    save_snapshot(&name).await
}

async fn update_interactive() -> Result<()> {
    let name = pick_machine(&store::machines(&store::dirs()), "update")?;
    save_snapshot(&name).await
}

async fn save_snapshot(vm: &str) -> Result<()> {
    let snapshot = store::snapshot_name(&store::dirs());
    println!("saving {vm} as {snapshot}");
    let mut client = connect().await?;
    let version = client.save_snapshot(vm, &snapshot).await?;
    println!("{snapshot} v{version}");
    Ok(())
}

async fn destroy_cmd(args: &mut impl Iterator<Item = String>) -> Result<()> {
    let mut name = None;
    let mut yes = false;
    for arg in args {
        if arg == "--yes" {
            yes = true;
        } else if name.is_none() {
            name = Some(arg);
        } else {
            bail!("usage: destroy [machine] [--yes]");
        }
    }
    let name = name.context("usage: destroy [machine] [--yes]")?;
    let owned = store::machines(&store::dirs());
    if !owned.iter().any(|existing| existing == &name) {
        bail!("{name} was not created by this plugin");
    }
    if !yes {
        print!("Destroy {name}? This deletes the computer. [y/N] ");
        io::stdout().flush().ok();
        if !matches!(read_line()?.as_str(), "y" | "yes") {
            println!("cancelled");
            return Ok(());
        }
    }
    destroy_machine(&name).await
}

async fn destroy_interactive() -> Result<()> {
    let name = pick_machine(&store::machines(&store::dirs()), "destroy")?;
    print!("Destroy {name}? This deletes the computer. [y/N] ");
    io::stdout().flush().ok();
    if !matches!(read_line()?.as_str(), "y" | "yes") {
        println!("cancelled");
        return Ok(());
    }
    destroy_machine(&name).await
}

async fn destroy_machine(name: &str) -> Result<()> {
    if let Some(id) = herdr_profile_id(name)? {
        let status = Command::new(herdr_bin())
            .args(["machine", "remove", &id])
            .status()
            .context("herdr machine remove")?;
        if !status.success() {
            bail!("herdr machine remove exited {status}");
        }
    }
    let mut client = connect().await?;
    client.destroy(name).await?;
    ssh::remove(&format!("{name}.boxd")).context("remove SSH config")?;
    store::forget(&store::dirs(), name).context("forget machine")?;
    println!("destroyed {name}");
    Ok(())
}

fn configure_interactive() -> Result<()> {
    let dirs = store::dirs();
    println!("snapshot is {}", store::snapshot_name(&dirs));
    print!("new snapshot name (blank keeps current): ");
    io::stdout().flush().ok();
    let name = read_line()?;
    if name.is_empty() {
        return Ok(());
    }
    store::set_snapshot(&dirs, &name).context("save snapshot name")?;
    println!("saved snapshot={name}");
    Ok(())
}

fn pick_machine(names: &[String], verb: &str) -> Result<String> {
    if names.is_empty() {
        bail!("no plugin machines to {verb}");
    }
    if names.len() == 1 && !io::stdin().is_terminal() {
        return Ok(names[0].clone());
    }
    for (index, name) in names.iter().enumerate() {
        println!("{}  {name}", index + 1);
    }
    print!("{verb} which? ");
    io::stdout().flush().ok();
    let choice = read_line()?;
    let index: usize = choice
        .parse()
        .with_context(|| format!("pick a number 1-{}", names.len()))?;
    names
        .get(index.checked_sub(1).context("pick starts at 1")?)
        .cloned()
        .with_context(|| format!("pick a number 1-{}", names.len()))
}

async fn connect() -> Result<boxd::Client> {
    let dirs = store::dirs();
    let key = store::api_key(&dirs);
    let token = boxd::load_token(key.as_deref())?;
    boxd::Client::connect(&token).await
}

fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into())
}

fn herdr_machine_add(target: &str, label: &str) -> Result<()> {
    let status = Command::new(herdr_bin())
        .args([
            "machine",
            "add",
            target,
            "--label",
            label,
            "--remote-session",
            "default",
        ])
        .status()
        .context("herdr machine add")?;
    if status.success() {
        Ok(())
    } else {
        bail!("herdr machine add exited {status}");
    }
}

fn herdr_profile_id(label: &str) -> Result<Option<String>> {
    let output = Command::new(herdr_bin())
        .args(["machine", "list", "--json"])
        .output()
        .context("herdr machine list")?;
    if !output.status.success() {
        bail!("herdr machine list exited {}", output.status);
    }
    let list: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).context("herdr machine list is not JSON")?;
    Ok(list.into_iter().find_map(|item| {
        let found = item.get("label").and_then(|value| value.as_str())?;
        if found != label {
            return None;
        }
        item.get("id")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    }))
}

#[derive(Clone, Copy)]
struct MachineSize {
    label: &'static str,
    vcpu: u32,
    memory_bytes: u64,
}

const GIB: u64 = 1024 * 1024 * 1024;
const DEFAULT_SIZE: MachineSize = MachineSize {
    label: "2x8G",
    vcpu: 2,
    memory_bytes: 8 * GIB,
};
const SIZES: [MachineSize; 4] = [
    MachineSize {
        label: "1x4G",
        vcpu: 1,
        memory_bytes: 4 * GIB,
    },
    DEFAULT_SIZE,
    MachineSize {
        label: "4x16G",
        vcpu: 4,
        memory_bytes: 16 * GIB,
    },
    MachineSize {
        label: "8x32G",
        vcpu: 8,
        memory_bytes: 32 * GIB,
    },
];

fn parse_size(raw: &str) -> Result<MachineSize> {
    let raw = raw.trim();
    let labeled = if raw.ends_with(['G', 'g']) {
        raw.to_owned()
    } else {
        format!("{raw}G")
    };
    SIZES
        .into_iter()
        .find(|size| size.label.eq_ignore_ascii_case(&labeled))
        .with_context(|| format!("size must be 1x4, 2x8, 4x16, or 8x32, not {raw}"))
}

fn ask_size() -> Result<MachineSize> {
    println!("Size: 1=1x4G  2=2x8G  3=4x16G  4=8x32G  [2]");
    print!("> ");
    io::stdout().flush().ok();
    match read_key()? {
        '1' => Ok(SIZES[0]),
        '2' | '\r' | '\n' => Ok(DEFAULT_SIZE),
        '3' => Ok(SIZES[2]),
        '4' => Ok(SIZES[3]),
        other => bail!("press 1-4, not {other}"),
    }
}

fn ask_machine_name() -> Result<String> {
    print!("Machine name (blank for the boxd default): ");
    io::stdout().flush().ok();
    let name = read_line()?;
    if name.is_empty() || is_machine_name(&name) {
        return Ok(name);
    }
    bail!("name must be lowercase letters, digits, and hyphens");
}

fn is_machine_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        && !name.ends_with('-')
}

fn read_line() -> Result<String> {
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).context("read")?;
    Ok(line.trim().to_owned())
}

fn read_key() -> Result<char> {
    if !io::stdin().is_terminal() {
        let line = read_line()?;
        return line.chars().next().context("empty input");
    }
    let fd = io::stdin().as_raw_fd();
    let mut original = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
        return Err(io::Error::last_os_error()).context("terminal attributes");
    }
    let mut raw = original;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return Err(io::Error::last_os_error()).context("raw mode");
    }
    let mut byte = [0u8; 1];
    let read = io::stdin().read_exact(&mut byte);
    unsafe {
        libc::tcsetattr(fd, libc::TCSANOW, &original);
    }
    read.context("read key")?;
    Ok(byte[0] as char)
}

fn show_error(err: &anyhow::Error) {
    println!("error: {err:#}");
    println!("press any key");
    let _ = io::stdout().flush();
    let _ = read_key();
}

#[cfg(test)]
mod tests {
    use super::{is_machine_name, parse_size};

    #[test]
    fn machine_name_accepts_herdr_box() {
        assert!(is_machine_name("herdr-box"));
    }

    #[test]
    fn parse_size_accepts_the_default_label() {
        assert_eq!(parse_size("2x8").unwrap().vcpu, 2);
    }

    #[test]
    fn machine_name_rejects_uppercase() {
        assert!(!is_machine_name("Herdr"));
    }
}
