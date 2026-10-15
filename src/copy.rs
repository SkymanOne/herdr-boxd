//! Packs a local project into a tarball and unpacks it in the machine.
//! In a git repo it sends tracked plus untracked-but-not-ignored files and
//! `.git`; elsewhere it skips the usual build and dependency directories.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::{boxd, store, ui, Coded};

const SKIP: [&str; 9] = [
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".turbo",
    ".cache",
    ".DS_Store",
];
pub const CONFIRM_BYTES: u64 = 500 * 1024 * 1024;

pub struct Packed {
    /// Basename used as the directory under /home/boxd.
    pub dest: String,
    pub bytes: u64,
    tar: TempFile,
}

impl Packed {
    pub fn remote_dir(&self) -> String {
        format!("/home/boxd/{}", self.dest)
    }
}

/// Deleted when dropped, so every exit path cleans up the tarball.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn pack(cwd: &Path) -> Result<Packed> {
    let dest = store::copy_destination(cwd).map_err(|msg| Coded::new("invalid", msg))?;
    let cwd = fs::canonicalize(cwd)?;
    let parent = cwd.parent().unwrap_or(Path::new("/")).to_owned();
    // create_new + 0600: no symlink games in a shared temp dir, and the
    // project contents stay private to this user.
    let tar_path = std::env::temp_dir().join(format!("herdr-boxd-{}.tar", store::random_hex(8)));
    let tar_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tar_path)
        .context("create temporary tarball")?;
    let tar = TempFile(tar_path);
    let files = git_files(&cwd);
    ui::step(if files.is_some() {
        format!("packing {dest} (git-tracked and untracked files, plus .git)")
    } else {
        format!("packing {dest} (skipping {})", SKIP.join(", "))
    });
    let mut cmd = Command::new("tar");
    // macOS tar would otherwise add ._* AppleDouble files and xattr headers.
    cmd.env("COPYFILE_DISABLE", "1");
    cmd.arg("--no-xattrs")
        .arg("-C")
        .arg(&parent)
        .arg("-cf")
        .arg("-");
    cmd.stdout(tar_file).stderr(Stdio::piped());
    let output = match &files {
        Some(files) => {
            cmd.args(["--null", "-T", "-"]).stdin(Stdio::piped());
            let mut child = cmd.spawn().context("run tar")?;
            let mut list = Vec::new();
            for file in files {
                list.extend_from_slice(format!("{dest}/{file}").as_bytes());
                list.push(0);
            }
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(&list).context("send file list to tar")?;
            }
            child.wait_with_output().context("wait for tar")?
        }
        None => {
            for skip in SKIP {
                cmd.arg("--exclude").arg(skip);
            }
            cmd.arg(&dest)
                .stdin(Stdio::null())
                .output()
                .context("run tar")?
        }
    };
    if !output.status.success() {
        bail!(
            "tar failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let bytes = fs::metadata(&tar.0)?.len();
    Ok(Packed { dest, bytes, tar })
}

/// Paths relative to `cwd`, or None outside a git work tree.
fn git_files(cwd: &Path) -> Option<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut files: Vec<String> = output
        .stdout
        .split(|b| *b == 0)
        .filter(|path| !path.is_empty())
        .filter_map(|path| std::str::from_utf8(path).ok())
        // Tracked files deleted in the work tree would make tar fail.
        .filter(|path| cwd.join(path).symlink_metadata().is_ok())
        .map(str::to_owned)
        .collect();
    if cwd.join(".git").exists() {
        files.push(".git".into());
    }
    Some(files)
}

pub fn describe(packed: &Packed) -> String {
    format!("{:.1} MB", packed.bytes as f64 / (1024.0 * 1024.0))
}

/// Uploads and unpacks into /home/boxd/<dest>, over any earlier copy.
pub async fn push(client: &mut boxd::Client, vm: &str, packed: &Packed) -> Result<String> {
    let remote = format!("/tmp/herdr-boxd-{}.tar", store::random_hex(8));
    ui::step(format!("uploading {} ({})", packed.dest, describe(packed)));
    client.upload_file(vm, &remote, &packed.tar.0).await?;
    client
        .exec(
            vm,
            &format!("set -e; mkdir -p /home/boxd; tar -xf {remote} -C /home/boxd; rm -f {remote}"),
        )
        .await?
        .check("unpacking the project")?;
    let dir = packed.remote_dir();
    ui::done(format!("copied to {dir}"));
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_in_git_repo_skips_ignored_files() {
        let root = std::env::temp_dir().join(format!("herdr-boxd-pack-{}", store::random_hex(4)));
        let repo = root.join("proj");
        fs::create_dir_all(repo.join("target")).unwrap();
        fs::write(repo.join("main.rs"), "fn main() {}").unwrap();
        fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        fs::write(repo.join("target/big"), "x").unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return;
        }
        let packed = pack(&repo).unwrap();
        let list = Command::new("tar")
            .arg("-tf")
            .arg(&packed.tar.0)
            .output()
            .unwrap();
        let list = String::from_utf8_lossy(&list.stdout);
        assert!(list.contains("proj/main.rs"));
        assert!(list.contains("proj/.git"));
        assert!(!list.contains("proj/target/big"));
        let tar = packed.tar.0.clone();
        drop(packed);
        assert!(!tar.exists());
        let _ = fs::remove_dir_all(&root);
    }
}
