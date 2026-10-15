//! SSH aliases for plugin machines. The boxd client daemon normally writes
//! `Host <name>.boxd` entries itself; this module only writes a fallback block
//! when `ssh -G` shows the alias does not resolve yet.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Result};

use crate::{store, ui, Coded};

pub fn alias(name: &str) -> String {
    format!("{name}.boxd")
}

pub fn identity_file() -> io::Result<PathBuf> {
    let home = home()?;
    for dir in [
        "Library/Application Support/boxd",
        ".config/boxd",
        ".local/share/boxd",
    ] {
        let Ok(entries) = fs::read_dir(home.join(dir)) else {
            continue;
        };
        let mut keys: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("id_ed25519_") && !n.ends_with(".pub"))
            })
            .collect();
        keys.sort();
        if let Some(key) = keys.into_iter().next() {
            return Ok(key);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no boxd SSH identity found; run `boxd auth login` once",
    ))
}

/// Makes `<name>.boxd` reach `host:port`. Returns true if a fallback block was
/// written (boxd's daemon had not added the alias yet).
pub fn ensure_alias(name: &str, host: &str, port: u32, host_key: &str) -> Result<bool> {
    if resolves_to(name, host, port) {
        return Ok(false);
    }
    let alias = alias(name);
    let identity = identity_file()?;
    let ssh_dir = home()?.join(".ssh");
    fs::create_dir_all(&ssh_dir)?;
    store::locked(&ssh_dir.join(".boxd-config.lock"), || {
        let path = ssh_dir.join("config");
        let existing = fs::read_to_string(&path).unwrap_or_default();
        let mut text = strip_block(&existing, &alias);
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&block(&alias, host, port, &identity));
        write_preserving_mode(&path, &text)?;
        if !host_key.is_empty() {
            append_known_host(&ssh_dir.join("known_hosts"), host, port, host_key)?;
        }
        Ok(())
    })?;
    Ok(true)
}

/// Waits until `ssh <alias> true` works, which proves DNS, the port, the host
/// key, and our identity in one go.
pub async fn wait_ready(name: &str, limit: Duration) -> Result<()> {
    let alias = alias(name);
    let deadline = tokio::time::Instant::now() + limit;
    let last = loop {
        let output = tokio::process::Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                "-o",
                "LogLevel=ERROR",
                &alias,
                "true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await?;
        if output.status.success() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            break String::from_utf8_lossy(&output.stderr).trim().to_owned();
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    bail!(Coded::new(
        "ssh_unreachable",
        format!("cannot SSH to {alias} after {}s: {last}", limit.as_secs()),
    ));
}

/// Removes our fallback block, and the mis-typed one older versions wrote.
pub fn remove(name: &str) -> io::Result<()> {
    let ssh_dir = home()?.join(".ssh");
    let path = ssh_dir.join("config");
    if !path.exists() {
        return Ok(());
    }
    store::locked(&ssh_dir.join(".boxd-config.lock"), || {
        let text = fs::read_to_string(&path)?;
        let stripped = strip_block(&text, &alias(name));
        if stripped != text {
            write_preserving_mode(&path, &stripped)?;
        }
        Ok(())
    })
}

/// True when `ssh <name>.boxd` already goes to `host:port`.
pub fn resolves_to(name: &str, host: &str, port: u32) -> bool {
    let Ok(output) = Command::new("ssh")
        .args(["-G", &alias(name)])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let value = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(' '))
            .map(str::to_owned)
    };
    value("hostname").as_deref() == Some(host) && value("port") == Some(port.to_string())
}

fn home() -> io::Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}

fn begin(alias: &str) -> String {
    format!("# herdr-boxd begin {alias}")
}

fn end(alias: &str) -> String {
    format!("# herdr-boxd end {alias}")
}

fn block(alias: &str, host: &str, port: u32, identity: &Path) -> String {
    format!(
        "{begin}\nHost {alias}\n    HostName {host}\n    Port {port}\n    User boxd\n    IdentityFile \"{identity}\"\n    IdentitiesOnly yes\n    ServerAliveInterval 30\n{end}\n",
        begin = begin(alias),
        end = end(alias),
        identity = identity.display(),
    )
}

fn strip_block(text: &str, alias: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in text.lines() {
        if line == begin(alias) {
            skipping = true;
            continue;
        }
        if skipping {
            if line == end(alias) {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if skipping {
        // Unterminated block: keep the original rather than eat the rest of the file.
        return text.to_owned();
    }
    out
}

fn write_preserving_mode(path: &Path, text: &str) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    store::write_atomic(path, text.as_bytes(), mode)
}

fn append_known_host(path: &Path, host: &str, port: u32, host_key: &str) -> io::Result<()> {
    let marker = format!("[{host}]:{port} ");
    let existing = fs::read_to_string(path).unwrap_or_default();
    if existing.lines().any(|line| line.starts_with(&marker)) {
        return Ok(());
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, "{marker}{host_key}")
}

pub fn warn_fallback(name: &str) {
    ui::note(format!(
        "added a {} entry to ~/.ssh/config (boxd's daemon had not written one yet)",
        alias(name)
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_block_removes_only_the_named_machine() {
        let text =
            "Host other\n    User me\n# herdr-boxd begin old.boxd\nHost old.boxd\n# herdr-boxd end old.boxd\n";
        let stripped = strip_block(text, "old.boxd");
        assert!(stripped.contains("Host other"));
        assert!(!stripped.contains("old.boxd"));
    }

    #[test]
    fn strip_block_keeps_text_when_the_end_marker_is_missing() {
        let text = "# herdr-boxd begin a.boxd\nHost a.boxd\nHost mine\n";
        assert_eq!(strip_block(text, "a.boxd"), text);
    }

    #[test]
    fn block_names_the_alias_and_real_host() {
        let text = block("a.boxd", "a.boxd.sh", 2222, Path::new("/k"));
        assert!(text.contains("Host a.boxd\n"));
        assert!(text.contains("HostName a.boxd.sh"));
        assert_eq!(strip_block(&text, "a.boxd"), "");
    }
}
