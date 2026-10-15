use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

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

fn config_path(dirs: &Dirs) -> PathBuf {
    dirs.config.join("config")
}

pub fn snapshot_name(dirs: &Dirs) -> String {
    read_value(&config_path(dirs), "snapshot").unwrap_or_else(|| DEFAULT_SNAPSHOT.to_owned())
}

pub fn set_snapshot(dirs: &Dirs, name: &str) -> io::Result<()> {
    write_value(&config_path(dirs), "snapshot", name)
}

pub fn api_key(dirs: &Dirs) -> Option<String> {
    read_value(&config_path(dirs), "api_key").filter(|value| !value.is_empty())
}

pub fn set_api_key(dirs: &Dirs, key: &str) -> io::Result<()> {
    write_value(&config_path(dirs), "api_key", key)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(config_path(dirs), fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn read_value(path: &Path, key: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let Some((found, value)) = line.split_once('=') else {
            continue;
        };
        if found == key {
            return Some(value.to_owned());
        }
    }
    None
}

fn write_value(path: &Path, key: &str, value: &str) -> io::Result<()> {
    if value.contains(['\n', '=']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "value cannot contain newline or '='",
        ));
    }
    let mut lines: Vec<String> = fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.starts_with(&format!("{key}=")))
        .map(str::to_owned)
        .collect();
    lines.push(format!("{key}={value}"));
    let mut file = fs::File::create(path)?;
    for line in lines {
        writeln!(file, "{line}")?;
    }
    Ok(())
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

pub fn remember(dirs: &Dirs, name: &str) -> io::Result<()> {
    let mut names = machines(dirs);
    if !names.iter().any(|existing| existing == name) {
        names.push(name.to_owned());
    }
    write_machines(dirs, &names)
}

pub fn forget(dirs: &Dirs, name: &str) -> io::Result<()> {
    let names: Vec<String> = machines(dirs)
        .into_iter()
        .filter(|existing| existing != name)
        .collect();
    write_machines(dirs, &names)
}

fn write_machines(dirs: &Dirs, names: &[String]) -> io::Result<()> {
    let mut file = fs::File::create(ledger_path(dirs))?;
    for name in names {
        writeln!(file, "{name}")?;
    }
    Ok(())
}

pub fn copy_destination(cwd: &Path) -> Result<String, String> {
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    if cwd.as_os_str().is_empty() || cwd == Path::new("/") || home.as_deref() == Some(cwd) {
        return Err("refusing to copy / or your home directory".into());
    }
    if !cwd.is_dir() {
        return Err(format!("{} is not a directory", cwd.display()));
    }
    let name = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && !name.contains('/') && *name != "." && *name != "..")
        .ok_or_else(|| format!("cannot name a remote directory for {}", cwd.display()))?;
    Ok(name.to_owned())
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

    #[test]
    fn snapshot_name_defaults_when_config_is_missing() {
        let root = std::env::temp_dir().join(format!("herdr-boxd-default-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dirs = Dirs {
            config: root.join("config"),
            state: root.join("state"),
        };
        fs::create_dir_all(&dirs.config).unwrap();
        fs::create_dir_all(&dirs.state).unwrap();
        assert_eq!(snapshot_name(&dirs), DEFAULT_SNAPSHOT);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn set_snapshot_and_api_key_round_trip() {
        let root = std::env::temp_dir().join(format!("herdr-boxd-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dirs = Dirs {
            config: root.join("config"),
            state: root.join("state"),
        };
        fs::create_dir_all(&dirs.config).unwrap();
        fs::create_dir_all(&dirs.state).unwrap();
        set_snapshot(&dirs, "herd-base").unwrap();
        set_api_key(&dirs, "bxd_test").unwrap();
        assert_eq!(snapshot_name(&dirs), "herd-base");
        assert_eq!(api_key(&dirs).as_deref(), Some("bxd_test"));
        remember(&dirs, "herd-a").unwrap();
        remember(&dirs, "herd-a").unwrap();
        remember(&dirs, "herd-b").unwrap();
        assert_eq!(machines(&dirs), ["herd-a", "herd-b"]);
        forget(&dirs, "herd-a").unwrap();
        assert_eq!(machines(&dirs), ["herd-b"]);
        let _ = fs::remove_dir_all(&root);
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
    fn copy_destination_uses_the_directory_basename() {
        let dir = std::env::temp_dir().join(format!("herdr-boxd-dir-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            copy_destination(&dir).unwrap(),
            dir.file_name().unwrap().to_str().unwrap()
        );
        let _ = fs::remove_dir(&dir);
    }
}
