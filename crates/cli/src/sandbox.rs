//! `codecraft sandbox setup|status`: install Firecracker, a kernel and a rootfs
//! into ~/.codecraft/sandbox. Uses curl, tar, unsquashfs and mkfs.ext4.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context};
use codecraft_tools::sandbox::{Firecracker, INIT};

const CI_BUCKET: &str = "http://spec.ccfc.min.s3.amazonaws.com";
const CI_VERSION: &str = "v1.15";

fn sh(cmd: &str) -> anyhow::Result<String> {
    let out = Command::new("bash").arg("-c").arg(cmd).output()?;
    if !out.status.success() {
        bail!("`{cmd}` failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn need(bin: &str, pkg: &str) -> anyhow::Result<()> {
    if codecraft_core::config::which(bin).is_none() && !Path::new(&format!("/usr/sbin/{bin}")).exists() {
        bail!("`{bin}` not found; install {pkg}");
    }
    Ok(())
}

pub async fn cmd(action: &str) -> anyhow::Result<()> {
    let dir = codecraft_core::config::home_dir().join("sandbox");
    match action {
        "status" => {
            match Firecracker::from_dir(&dir) {
                Some(_) => println!("firecracker sandbox ready in {}", dir.display()),
                None if !Path::new("/dev/kvm").exists() => println!("no /dev/kvm: firecracker needs KVM (Linux)"),
                None => println!("not installed; run `codecraft sandbox setup`"),
            }
            Ok(())
        }
        "setup" => setup(&dir),
        _ => bail!("usage: codecraft sandbox setup|status"),
    }
}

fn setup(dir: &Path) -> anyhow::Result<()> {
    if !Path::new("/dev/kvm").exists() {
        bail!("/dev/kvm not found; Firecracker needs KVM");
    }
    for (b, p) in [("curl", "curl"), ("tar", "tar"), ("unsquashfs", "squashfs-tools"), ("mkfs.ext4", "e2fsprogs"), ("debugfs", "e2fsprogs")] {
        need(b, p)?;
    }
    std::fs::create_dir_all(dir)?;
    let d = dir.display();
    let arch = std::env::consts::ARCH;

    if !dir.join("firecracker").exists() {
        println!("downloading firecracker…");
        let tag = sh("curl -fsSL https://api.github.com/repos/firecracker-microvm/firecracker/releases/latest | grep -oP '\"tag_name\":\\s*\"\\K[^\"]+'")?;
        sh(&format!(
            "cd {d} && curl -fsSL -o fc.tgz https://github.com/firecracker-microvm/firecracker/releases/download/{tag}/firecracker-{tag}-{arch}.tgz \
             && tar xzf fc.tgz && cp release-{tag}-{arch}/firecracker-{tag}-{arch} firecracker && rm -rf fc.tgz release-{tag}-{arch}"
        ))?;
    }
    if !dir.join("vmlinux").exists() {
        println!("downloading kernel…");
        let key = sh(&format!(
            "curl -fsSL '{CI_BUCKET}/?prefix=firecracker-ci/{CI_VERSION}/{arch}/vmlinux-&list-type=2' | grep -oP '(?<=<Key>)(firecracker-ci/{CI_VERSION}/{arch}/vmlinux-[0-9]+\\.[0-9]+\\.[0-9]{{1,3}})(?=</Key>)' | sort -V | tail -1"
        ))?;
        if key.is_empty() {
            bail!("could not find a Firecracker CI kernel");
        }
        sh(&format!("curl -fsSL -o {d}/vmlinux https://s3.amazonaws.com/spec.ccfc.min/{key}"))?;
    }
    if !dir.join("rootfs.ext4").exists() {
        println!("downloading Ubuntu rootfs and building image…");
        let key = sh(&format!(
            "curl -fsSL '{CI_BUCKET}/?prefix=firecracker-ci/{CI_VERSION}/{arch}/ubuntu-&list-type=2' | grep -oP '(?<=<Key>)(firecracker-ci/{CI_VERSION}/{arch}/ubuntu-[0-9]+\\.[0-9]+\\.squashfs)(?=</Key>)' | sort -V | tail -1"
        ))?;
        if key.is_empty() {
            bail!("could not find a Firecracker CI rootfs");
        }
        sh(&format!("curl -fsSL -o {d}/rootfs.squashfs https://s3.amazonaws.com/spec.ccfc.min/{key}"))?;
        sh(&format!("rm -rf {d}/root && unsquashfs -q -d {d}/root {d}/rootfs.squashfs >/dev/null"))?;
        std::fs::write(dir.join("root/cc-init"), INIT)?;
        sh(&format!("chmod +x {d}/root/cc-init && mkdir -p {d}/root/work"))?;
        sh(&format!("mkfs.ext4 -q -d {d}/root {d}/rootfs.ext4 2G && rm -rf {d}/root {d}/rootfs.squashfs"))
            .context("building rootfs.ext4")?;
    }
    println!("sandbox ready in {d}. Use `codecraft --sandbox` or `sandbox = \"firecracker\"` in config.toml.");
    Ok(())
}
