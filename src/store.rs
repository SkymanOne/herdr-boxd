use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const PLUGIN_ID: &str = "boxd.session";
pub const DEFAULT_SNAPSHOT: &str = "herd-base";

pub struct Dirs {
    pub config: PathBuf,
    pub state: PathBuf,
}

pub fn dirs() -> Dirs {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let config = std::env::var("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(&home)
                .join(".config/herdr/plugins/config")
                .join(PLUGIN_ID)
        });
    let state = std::env::var("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(&home)
                .join(".local/state/herdr/plugins")
                .join(PLUGIN_ID)
        });
    fs::create_dir_all(&config).ok();
    fs::create_dir_all(&state).ok();
    Dirs { config, state }
}

/// Replaces `path` in one rename so a crash never leaves it half written.
/// Follows a symlinked `path` so dotfile managers keep their link.
pub fn write_atomic(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        random_hex(6)
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Runs `f` while holding an exclusive flock on `lock`. Parallel agent spawns
/// would otherwise lose each other's ledger writes.
pub fn locked<T>(lock: &Path, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(lock)?;
    // SAFETY: flock on a descriptor we own; released when `file` drops.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    f()
}

fn state_lock(dirs: &Dirs) -> PathBuf {
    dirs.state.join(".lock")
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    if fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_err()
    {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let seed = nanos ^ u128::from(std::process::id());
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (seed >> ((i % 16) * 8)) as u8;
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn uuid_v4() -> String {
    let h = random_hex(16);
    let variant = ["8", "9", "a", "b"][usize::from_str_radix(&h[16..17], 16).unwrap_or(0) % 4];
    format!(
        "{}-{}-4{}-{}{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        variant,
        &h[17..20],
        &h[20..32]
    )
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn config_path(dirs: &Dirs) -> PathBuf {
    dirs.config.join("config")
}

pub fn snapshot_name(dirs: &Dirs) -> String {
    read_value(&config_path(dirs), "snapshot")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_SNAPSHOT.to_owned())
}

pub fn set_snapshot(dirs: &Dirs, name: &str) -> io::Result<()> {
    if name.is_empty() || name.chars().any(char::is_whitespace) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "snapshot name cannot be empty or contain spaces",
        ));
    }
    write_value(&config_path(dirs), "snapshot", Some(name))
}

pub fn api_key(dirs: &Dirs) -> Option<String> {
    read_value(&config_path(dirs), "api_key").filter(|value| !value.is_empty())
}

pub fn set_api_key(dirs: &Dirs, key: Option<&str>) -> io::Result<()> {
    if let Some(key) = key {
        if key.is_empty() || key.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "API key cannot be empty or contain spaces",
            ));
        }
    }
    write_value(&config_path(dirs), "api_key", key)
}

fn read_value(path: &Path, key: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .filter_map(|line| line.split_once('='))
        .find(|(found, _)| *found == key)
        .map(|(_, value)| value.to_owned())
}

/// The config can hold an API key, so it is always written 0600.
fn write_value(path: &Path, key: &str, value: Option<&str>) -> io::Result<()> {
    if value.is_some_and(|v| v.contains(['\n', '\r', '='])) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "value cannot contain a newline or '='",
        ));
    }
    let prefix = format!("{key}=");
    let mut text: String = fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.starts_with(&prefix))
        .map(|line| format!("{line}\n"))
        .collect();
    if let Some(value) = value {
        text.push_str(&format!("{prefix}{value}\n"));
    }
    write_atomic(path, text.as_bytes(), 0o600)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

fn ledger_path(dirs: &Dirs) -> PathBuf {
    dirs.state.join("machines")
}

pub fn machines(dirs: &Dirs) -> Vec<String> {
    fs::read_to_string(ledger_path(dirs))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

pub fn owns(dirs: &Dirs, name: &str) -> bool {
    machines(dirs).iter().any(|existing| existing == name)
}

pub fn remember(dirs: &Dirs, name: &str) -> io::Result<()> {
    locked(&state_lock(dirs), || {
        let mut names = machines(dirs);
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_owned());
        }
        write_machines(dirs, &names)
    })
}

pub fn forget(dirs: &Dirs, name: &str) -> io::Result<()> {
    locked(&state_lock(dirs), || {
        let names: Vec<String> = machines(dirs)
            .into_iter()
            .filter(|existing| existing != name)
            .collect();
        write_machines(dirs, &names)?;
        let agents: Vec<Agent> = load_agents(dirs)?
            .into_iter()
            .filter(|agent| agent.machine != name)
            .collect();
        write_agents(dirs, &agents)
    })
}

fn write_machines(dirs: &Dirs, names: &[String]) -> io::Result<()> {
    let text: String = names.iter().map(|name| format!("{name}\n")).collect();
    write_atomic(&ledger_path(dirs), text.as_bytes(), 0o644)
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Pane,
    Headless,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Agent {
    pub name: String,
    pub machine: String,
    pub kind: String,
    pub mode: Mode,
    pub workdir: String,
    /// Pane mode: the remote Herdr workspace that hosts the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Headless mode: the agent CLI's session id, for follow-up prompts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Native agent arguments, reused for headless follow-up prompts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// True when the machine was created for this agent.
    #[serde(default)]
    pub owns_machine: bool,
    pub created: u64,
}

fn agents_path(dirs: &Dirs) -> PathBuf {
    dirs.state.join("agents.json")
}

/// Read-only view; an unreadable ledger shows as empty here, but every write
/// goes through `load_agents` and refuses to overwrite it.
pub fn agents(dirs: &Dirs) -> Vec<Agent> {
    load_agents(dirs).unwrap_or_default()
}

fn load_agents(dirs: &Dirs) -> io::Result<Vec<Agent>> {
    let path = agents_path(dirs);
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} is unreadable ({err}); fix or move it aside",
                    path.display()
                ),
            )
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err),
    }
}

pub fn agent(dirs: &Dirs, name: &str) -> Option<Agent> {
    agents(dirs).into_iter().find(|agent| agent.name == name)
}

/// Inserts or replaces by name.
pub fn save_agent(dirs: &Dirs, agent: &Agent) -> io::Result<()> {
    locked(&state_lock(dirs), || {
        let mut all = load_agents(dirs)?;
        all.retain(|existing| existing.name != agent.name);
        all.push(agent.clone());
        write_agents(dirs, &all)
    })
}

/// Records `agent` only if the name is free, so two parallel spawns cannot
/// both claim it.
pub fn claim_agent(dirs: &Dirs, agent: &Agent) -> io::Result<bool> {
    locked(&state_lock(dirs), || {
        let mut all = load_agents(dirs)?;
        if all.iter().any(|existing| existing.name == agent.name) {
            return Ok(false);
        }
        all.push(agent.clone());
        write_agents(dirs, &all).map(|()| true)
    })
}

pub fn forget_agent(dirs: &Dirs, name: &str) -> io::Result<()> {
    locked(&state_lock(dirs), || {
        let all: Vec<Agent> = load_agents(dirs)?
            .into_iter()
            .filter(|agent| agent.name != name)
            .collect();
        write_agents(dirs, &all)
    })
}

fn write_agents(dirs: &Dirs, agents: &[Agent]) -> io::Result<()> {
    let text = serde_json::to_string_pretty(agents).map_err(io::Error::other)?;
    write_atomic(&agents_path(dirs), text.as_bytes(), 0o600)
}

pub fn copy_destination(cwd: &Path) -> Result<String, String> {
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    let cwd = fs::canonicalize(cwd).map_err(|err| format!("{}: {err}", cwd.display()))?;
    let home = home.and_then(|home| fs::canonicalize(home).ok());
    if cwd == Path::new("/") || home.as_deref() == Some(cwd.as_path()) {
        return Err("refusing to copy / or your home directory".into());
    }
    if !cwd.is_dir() {
        return Err(format!("{} is not a directory", cwd.display()));
    }
    let name = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| is_safe_dir_name(name))
        .ok_or_else(|| format!("cannot name a remote directory for {}", cwd.display()))?;
    Ok(name.to_owned())
}

/// The basename ends up in remote shell commands, so keep it boring.
fn is_safe_dir_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '@'))
}

pub fn context_cwd() -> Option<PathBuf> {
    if let Some(cwd) = std::env::var("BOXD_CWD")
        .ok()
        .filter(|value| !value.is_empty())
    {
        return Some(PathBuf::from(cwd));
    }
    let raw = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    json.get("focused_pane_cwd")
        .or_else(|| json.get("workspace_cwd"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dirs(tag: &str) -> (PathBuf, Dirs) {
        let root = std::env::temp_dir().join(format!("herdr-boxd-{tag}-{}", random_hex(4)));
        let dirs = Dirs {
            config: root.join("config"),
            state: root.join("state"),
        };
        fs::create_dir_all(&dirs.config).unwrap();
        fs::create_dir_all(&dirs.state).unwrap();
        (root, dirs)
    }

    #[test]
    fn snapshot_name_defaults_when_config_is_missing() {
        let (root, dirs) = temp_dirs("default");
        assert_eq!(snapshot_name(&dirs), DEFAULT_SNAPSHOT);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn config_round_trips_and_is_private() {
        let (root, dirs) = temp_dirs("config");
        set_snapshot(&dirs, "herd-base").unwrap();
        set_api_key(&dirs, Some("bxd_test")).unwrap();
        assert_eq!(snapshot_name(&dirs), "herd-base");
        assert_eq!(api_key(&dirs).as_deref(), Some("bxd_test"));
        let mode = fs::metadata(config_path(&dirs))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        set_api_key(&dirs, None).unwrap();
        assert_eq!(api_key(&dirs), None);
        assert_eq!(snapshot_name(&dirs), "herd-base");
        assert!(set_snapshot(&dirs, "bad name").is_err());
        assert!(set_api_key(&dirs, Some("a=b")).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ledger_tracks_machines_and_drops_their_agents() {
        let (root, dirs) = temp_dirs("ledger");
        remember(&dirs, "herd-a").unwrap();
        remember(&dirs, "herd-a").unwrap();
        remember(&dirs, "herd-b").unwrap();
        assert_eq!(machines(&dirs), ["herd-a", "herd-b"]);
        let agent = Agent {
            name: "fixer".into(),
            machine: "herd-a".into(),
            kind: "claude".into(),
            mode: Mode::Pane,
            workdir: "/home/boxd".into(),
            workspace: Some("w3".into()),
            session: None,
            args: Vec::new(),
            owns_machine: true,
            created: 1,
        };
        assert!(claim_agent(&dirs, &agent).unwrap());
        assert!(!claim_agent(&dirs, &agent).unwrap());
        assert_eq!(agent_names(&dirs), ["fixer"]);
        forget(&dirs, "herd-a").unwrap();
        assert_eq!(machines(&dirs), ["herd-b"]);
        assert!(agents(&dirs).is_empty());
        fs::write(agents_path(&dirs), "not json").unwrap();
        assert!(claim_agent(&dirs, &agent).is_err());
        assert_eq!(fs::read_to_string(agents_path(&dirs)).unwrap(), "not json");
        let _ = fs::remove_dir_all(&root);
    }

    fn agent_names(dirs: &Dirs) -> Vec<String> {
        agents(dirs).into_iter().map(|a| a.name).collect()
    }

    #[test]
    fn uuid_has_v4_shape() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!("89ab".contains(&id[19..20]));
    }

    #[test]
    fn copy_destination_rejects_filesystem_root() {
        assert!(copy_destination(Path::new("/")).is_err());
    }

    #[test]
    fn copy_destination_rejects_home_directory() {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        assert!(copy_destination(Path::new(&home)).is_err());
    }

    #[test]
    fn copy_destination_rejects_shell_metacharacters() {
        assert!(!is_safe_dir_name("a;rm -rf ~"));
        assert!(!is_safe_dir_name("$(x)"));
        assert!(is_safe_dir_name("herdr-boxd_v2.1"));
    }

    #[test]
    fn copy_destination_uses_the_directory_basename() {
        let dir = std::env::temp_dir().join(format!("herdr-boxd-dir-{}", random_hex(4)));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            copy_destination(&dir).unwrap(),
            dir.file_name().unwrap().to_str().unwrap()
        );
        let _ = fs::remove_dir(&dir);
    }
}
