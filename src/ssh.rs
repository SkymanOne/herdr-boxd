use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub struct SshTarget {
    pub alias: String,
    pub hostname: String,
    pub port: u32,
    pub identity: PathBuf,
}

pub fn identity_file() -> io::Result<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(&home).join("Library/Application Support/boxd"));
        dirs.push(PathBuf::from(&home).join(".config/boxd"));
        dirs.push(PathBuf::from(&home).join(".local/share/boxd"));
    }
    for dir in dirs {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("id_ed25519_") && !name.ends_with(".pub") {
                return Ok(entry.path());
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no boxd SSH identity found; run boxd auth login once",
    ))
}

pub fn install(target: &SshTarget, host_key: &str) -> io::Result<()> {
    let ssh_dir = home()?.join(".ssh");
    fs::create_dir_all(&ssh_dir)?;
    rewrite_config(&ssh_dir.join("config"), target)?;
    if !host_key.is_empty() {
        append_known_host(&ssh_dir.join("known_hosts"), target, host_key)?;
    }
    Ok(())
}

pub fn remove(alias: &str) -> io::Result<()> {
    let path = home()?.join(".ssh/config");
    if !path.exists() {
        return Ok(());
    }
    let text = fs::read_to_string(&path)?;
    fs::write(&path, strip_block(&text, alias))?;
    Ok(())
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
    out
}

fn rewrite_config(path: &Path, target: &SshTarget) -> io::Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let mut text = strip_block(&existing, &target.alias);
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&begin(&target.alias));
    text.push('\n');
    text.push_str(&format!(
        "Host {alias} {alias}.boxd.sh\n    HostName {host}\n    Port {port}\n    User boxd\n    IdentityFile \"{identity}\"\n    IdentitiesOnly yes\n",
        alias = target.alias,
        host = target.hostname,
        port = target.port,
        identity = target.identity.display(),
    ));
    text.push_str(&end(&target.alias));
    text.push('\n');
    let mut file = fs::File::create(path)?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

fn append_known_host(path: &Path, target: &SshTarget, host_key: &str) -> io::Result<()> {
    let marker = format!("[{}]:{}", target.hostname, target.port);
    let existing = fs::read_to_string(path).unwrap_or_default();
    if existing.lines().any(|line| line.starts_with(&marker)) {
        return Ok(());
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{marker} {host_key}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_block_removes_only_the_named_machine() {
        let text =
            "Host other\n    User me\n# herdr-boxd begin old\nHost old\n# herdr-boxd end old\n";
        let stripped = strip_block(text, "old");
        assert!(stripped.contains("Host other"));
        assert!(!stripped.contains("begin old"));
    }
}
