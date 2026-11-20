//! Firecracker microVM sandbox for shell commands (Linux + KVM only).
//!
//! Each command boots a fresh microVM (~2 s) with:
//! - a read-only Ubuntu rootfs whose init (`/cc-init`) runs the command,
//! - the workspace copied into an ext4 image mounted at /work,
//! - no network device.
//!
//! After the VM exits, the image is unpacked with `debugfs` and changed files
//! are synced back to the real workspace (`.git` is never written back).
//! Nothing here needs root: images are built with `mkfs.ext4 -d`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub const INIT: &str = r#"#!/bin/sh
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev 2>/dev/null
mount -t tmpfs tmpfs /tmp 2>/dev/null
mount /dev/vdb /work
export HOME=/tmp PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
cd "/work/$(cat /work/.cc-dir 2>/dev/null)" && /bin/sh -c "$(cat /work/.cc-cmd)" > /work/.cc-out 2>&1
echo $? > /work/.cc-exit
cd /; rm -f /work/.cc-cmd /work/.cc-dir; sync; umount /work; reboot -f
"#;

#[derive(Debug, Clone)]
pub struct Firecracker {
    pub firecracker: PathBuf,
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub vcpus: u32,
    pub mem_mib: u32,
}

pub struct SandboxResult {
    pub output: String,
    pub exit_code: Option<i32>,
    pub changed: Vec<String>,
    pub deleted: Vec<String>,
}

impl Firecracker {
    /// Look for an installed sandbox in `dir` (see `codecraft sandbox setup`).
    pub fn from_dir(dir: &Path) -> Option<Firecracker> {
        let fc = dir.join("firecracker");
        let k = dir.join("vmlinux");
        let r = dir.join("rootfs.ext4");
        (fc.is_file() && k.is_file() && r.is_file() && Path::new("/dev/kvm").exists()).then(|| Firecracker {
            firecracker: fc,
            kernel: k,
            rootfs: r,
            vcpus: 2,
            mem_mib: 1024,
        })
    }

    pub async fn run(
        &self,
        workspace: &Path,
        subdir: &str,
        cmd: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> anyhow::Result<SandboxResult> {
        let tmp = tempfile::tempdir()?;
        let stage = tmp.path().join("stage");
        // 1. Snapshot the workspace (respecting .gitignore would break builds; copy everything but huge caches).
        let before = copy_tree(workspace, &stage)?;
        std::fs::write(stage.join(".cc-cmd"), cmd)?;
        std::fs::write(stage.join(".cc-dir"), subdir)?;
        let size_mb = (dir_size(&stage) / (1024 * 1024)) * 2 + 512;
        let img = tmp.path().join("work.ext4");
        let st = Command::new("mkfs.ext4")
            .args(["-q", "-F", "-d"])
            .arg(&stage)
            .arg(&img)
            .arg(format!("{size_mb}M"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .context("mkfs.ext4 (install e2fsprogs)")?;
        if !st.success() {
            bail!("mkfs.ext4 failed");
        }
        // 2. Boot.
        let config = serde_json::json!({
            "boot-source": {
                "kernel_image_path": self.kernel,
                "boot_args": "console=ttyS0 reboot=k panic=1 pci=off quiet loglevel=0 init=/cc-init"
            },
            "drives": [
                {"drive_id": "rootfs", "path_on_host": self.rootfs, "is_root_device": true, "is_read_only": true},
                {"drive_id": "work", "path_on_host": img, "is_root_device": false, "is_read_only": false}
            ],
            "machine-config": {"vcpu_count": self.vcpus, "mem_size_mib": self.mem_mib}
        });
        let cfg = tmp.path().join("vm.json");
        std::fs::write(&cfg, config.to_string())?;
        let mut child = Command::new(&self.firecracker)
            .arg("--no-api")
            .arg("--config-file")
            .arg(&cfg)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("start firecracker")?;
        let mut note = String::new();
        tokio::select! {
            _ = child.wait() => {}
            _ = tokio::time::sleep(timeout) => { let _ = child.kill().await; note = format!("\n[killed: timed out after {}s]", timeout.as_secs()); }
            _ = cancel.cancelled() => { let _ = child.kill().await; note = "\n[killed: interrupted by user]".into(); }
        }
        // 3. Unpack and sync back.
        let out_dir = tmp.path().join("out");
        std::fs::create_dir_all(&out_dir)?;
        let st = Command::new("debugfs")
            .arg("-R")
            .arg(format!("rdump / {}", out_dir.display()))
            .arg(&img)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .context("debugfs (install e2fsprogs)")?;
        if !st.success() {
            bail!("debugfs failed to read the workspace image");
        }
        let output = std::fs::read_to_string(out_dir.join(".cc-out")).unwrap_or_default() + &note;
        let exit_code = std::fs::read_to_string(out_dir.join(".cc-exit")).ok().and_then(|s| s.trim().parse().ok());
        for f in [".cc-out", ".cc-exit", ".cc-cmd", ".cc-dir"] {
            let _ = std::fs::remove_file(out_dir.join(f));
        }
        let _ = std::fs::remove_dir(out_dir.join("lost+found"));
        let (changed, deleted) = sync_back(&out_dir, workspace, &before)?;
        Ok(SandboxResult { output, exit_code, changed, deleted })
    }
}

const SKIP_DIRS: &[&str] = &["node_modules/.cache", ".venv/.cache"];

/// Copy `src` into `dst`; return a map of relative path -> (len, mtime) for change detection.
fn copy_tree(src: &Path, dst: &Path) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let mut seen = BTreeMap::new();
    for e in walk(src) {
        let rel = e.strip_prefix(src).unwrap().to_string_lossy().to_string();
        if SKIP_DIRS.iter().any(|s| rel.starts_with(s)) {
            continue;
        }
        let to = dst.join(&rel);
        let meta = std::fs::symlink_metadata(&e)?;
        if meta.is_dir() {
            std::fs::create_dir_all(&to)?;
        } else if meta.file_type().is_symlink() {
            if let Some(p) = to.parent() {
                std::fs::create_dir_all(p)?;
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(&e)?, &to)?;
        } else if meta.is_file() {
            if let Some(p) = to.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::copy(&e, &to)?;
            seen.insert(rel, hash_file(&e));
        }
    }
    std::fs::create_dir_all(dst)?;
    Ok(seen)
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            out.push(p.clone());
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(p);
            }
        }
    }
    out
}

fn hash_file(p: &Path) -> Vec<u8> {
    // Cheap content fingerprint: the bytes themselves for small files, len+head+tail for big ones.
    match std::fs::read(p) {
        Ok(b) if b.len() <= 1 << 20 => b,
        Ok(b) => {
            let mut v = (b.len() as u64).to_le_bytes().to_vec();
            v.extend_from_slice(&b[..4096]);
            v.extend_from_slice(&b[b.len() - 4096..]);
            v
        }
        Err(_) => vec![],
    }
}

fn dir_size(p: &Path) -> u64 {
    walk(p).iter().filter_map(|e| std::fs::symlink_metadata(e).ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
}

fn sync_back(
    out: &Path,
    workspace: &Path,
    before: &BTreeMap<String, Vec<u8>>,
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let mut changed = Vec::new();
    let mut after = std::collections::BTreeSet::new();
    for e in walk(out) {
        let rel = e.strip_prefix(out).unwrap().to_string_lossy().to_string();
        if rel == ".git" || rel.starts_with(".git/") {
            continue;
        }
        let meta = std::fs::symlink_metadata(&e)?;
        if !meta.is_file() {
            if meta.is_dir() {
                std::fs::create_dir_all(workspace.join(&rel))?;
            }
            continue;
        }
        after.insert(rel.clone());
        let new = hash_file(&e);
        if before.get(&rel) != Some(&new) {
            let to = workspace.join(&rel);
            if let Some(p) = to.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::copy(&e, &to)?;
            changed.push(rel);
        }
    }
    let mut deleted = Vec::new();
    for rel in before.keys() {
        if rel.starts_with(".git/") || after.contains(rel) {
            continue;
        }
        if std::fs::remove_file(workspace.join(rel)).is_ok() {
            deleted.push(rel.clone());
        }
    }
    Ok((changed, deleted))
}
