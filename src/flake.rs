use anyhow::{anyhow, bail, Context, Result};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::cmd::{self, Ctx};
use crate::disk;

/// Where the dotfiles flake lives on the live ISO, per hosts/live.nix:
/// `isoImage.contents` embeds the flake at /iso/dotfiles.
const ISO_FLAKE: &str = "/iso/dotfiles";

/// Resolve the flake source directory: explicit --flake, else /iso/dotfiles,
/// else a `./dotfiles` / `.` checkout containing flake.nix (dev/testing).
pub fn resolve_flake(explicit: Option<&str>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        let pb = PathBuf::from(p);
        check_flake_dir(&pb)?;
        return Ok(pb);
    }
    for cand in [
        ISO_FLAKE,
        "/iso/dotfiles",
        "./dotfiles",
        ".",
        "/home/loeclos/.dotfiles",
        "/home/nixos/.dotfiles",
    ] {
        let pb = PathBuf::from(cand);
        if pb.join("flake.nix").is_file() {
            // "." only counts if it looks like the dotfiles repo (hosts/ present).
            if cand == "." && !pb.join("hosts").is_dir() {
                continue;
            }
            return Ok(pb);
        }
    }
    bail!("no flake found: pass --flake <path> (live ISO embeds it at {ISO_FLAKE})")
}

fn check_flake_dir(p: &Path) -> Result<()> {
    if !p.is_dir() {
        bail!("--flake {} is not a directory", p.display());
    }
    if !p.join("flake.nix").is_file() {
        bail!("--flake {} contains no flake.nix", p.display());
    }
    Ok(())
}

/// Validate a flake host / path-segment name.
///
/// Host names are joined into paths (`hosts/<host>.nix`), so they must be a
/// single safe segment: 1-64 chars of `[a-z0-9_-]`, starting alphanumeric.
/// Rejects `/`, `..`, absolute paths, empty, and anything containing a
/// separator, dot-leading name, or whitespace.
pub fn valid_host_name(host: &str) -> Result<()> {
    if host.is_empty() || host.len() > 64 {
        bail!("host name {host:?} must be 1-64 characters");
    }
    if !host.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        bail!("host name {host:?} must start with an alphanumeric character");
    }
    if !host
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        bail!("host name {host:?} may only contain lowercase letters, digits, '-' and '_'");
    }
    Ok(())
}

/// Hosts available in the flake: hosts/<name>.nix minus live-* and common.
pub fn available_hosts(flake: &Path) -> Vec<String> {
    let mut hosts = Vec::new();
    let Ok(entries) = std::fs::read_dir(flake.join("hosts")) else {
        return hosts;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("nix") {
            continue;
        }
        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
            if stem == "common" || stem == "live" || stem.starts_with("live-") {
                continue;
            }
            hosts.push(stem.to_string());
        }
    }
    hosts.sort();
    hosts
}

/// Guess the install host from the live ISO hostname:
/// live-desktop -> desktop, live-laptop -> laptop.
pub fn guess_host_from_live() -> Option<String> {
    let hn = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    hn.strip_prefix("live-").map(|s| s.to_string())
}

/// Copy the (possibly read-only, store-backed /iso/dotfiles) flake to a
/// writable temp dir so we can refresh the hardware file before install.
/// Returns the tempdir (kept alive by caller) + the copied path.
///
/// Dry-run: no temp dir, no copy — a placeholder display path is returned so
/// the printed plan still shows where staging would happen.
pub fn stage_writable_copy(ctx: Ctx, src: &Path) -> Result<(Option<tempfile::TempDir>, PathBuf)> {
    if ctx.dry_run {
        let dst = PathBuf::from("<dry-run staging dir>/dotfiles");
        println!(
            "(dry-run: would copy {} -> {} then chmod -R u+w)",
            src.display(),
            dst.display()
        );
        return Ok((None, dst));
    }
    let tmp = tempfile::Builder::new()
        .prefix("install-nix-flake-")
        .tempdir()
        .context("failed to create temp dir for flake staging")?;
    let dst = tmp.path().join("dotfiles");
    // cp -a preserves permissions/symlinks (wallpapers symlink etc.).
    cmd::run(
        ctx,
        "cp",
        &[
            "-a",
            &format!("{}", src.display()),
            &format!("{}", dst.display()),
        ],
    )?;
    // The store copy can be read-only (r-x); make the staging copy writable.
    cmd::run(ctx, "chmod", &["-R", "u+w", &format!("{}", dst.display())])?;
    detach_git(&dst)?;
    Ok((Some(tmp), dst))
}

/// A copied `.git` directory makes Nix treat the staged tree as a *git flake*,
/// where evaluation only sees tracked+committed files. The newly generated
/// `hosts/hardware/<host>.nix` would be untracked — hence invisible to the
/// installer, leaving the installed system with the OLD (wrong-disk) UUIDs.
/// Rename `.git` out of the way so the tree evaluates as a plain path flake:
/// every file on disk is visible. The original stays next to it for debugging.
fn detach_git(dst: &Path) -> Result<()> {
    let git = dst.join(".git");
    if git.is_dir() {
        let bak = PathBuf::from(format!("{}.install-nix-bak", git.display()));
        std::fs::rename(&git, &bak)
            .with_context(|| format!("could not move {} aside", git.display()))?;
        println!(
            "staged flake: renamed .git -> .install-nix-bak (so generated files are visible to flake evaluation)"
        );
    }
    Ok(())
}

/// Resolve `hosts/hardware/<host>.nix` inside the staged flake.
///
/// Safety: the host name is a validated path segment, the directory is
/// canonicalized and required to live inside the staging dir, and an existing
/// symlink at the target (which staging could have copied from upstream) is
/// refused instead of followed — writing through it would land outside the
/// staged copy.
fn hardware_file_path(staged_flake: &Path, host: &str) -> Result<PathBuf> {
    valid_host_name(host)?;
    let staged_root = std::fs::canonicalize(staged_flake)
        .with_context(|| format!("cannot canonicalize {}", staged_flake.display()))?;
    let hw_dir = staged_root.join("hosts/hardware");
    let hw_real = std::fs::canonicalize(&hw_dir)
        .with_context(|| format!("no hosts/hardware directory in {}", staged_root.display()))?;
    if !hw_real.starts_with(&staged_root) {
        bail!(
            "hosts/hardware resolves to {}, outside the staged flake {}",
            hw_real.display(),
            staged_root.display()
        );
    }
    let target = hw_real.join(format!("{host}.nix"));
    if std::fs::symlink_metadata(&target)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        bail!(
            "refusing: {} is a symlink; writing through it would escape the staged flake",
            target.display()
        );
    }
    Ok(target)
}

/// Refresh hosts/hardware/<host>.nix in the staged flake for the new disk.
///
/// Primary: `nixos-generate-config --root /mnt --dir <tmp>` (authoritative
/// kernel modules + UUIDs from the freshly mounted filesystems), then copy
/// the generated hardware-configuration.nix over the host's hardware file.
/// Fallback: synthesize a minimal hardware file from blkid UUIDs.
///
/// Dry-run: creates no temp dirs, runs nothing; prints the would-be target.
pub fn refresh_hardware_config(
    ctx: Ctx,
    staged_flake: &Path,
    host: &str,
    layout: &disk::Layout,
) -> Result<()> {
    if ctx.dry_run {
        println!(
            "(dry-run: would run nixos-generate-config --root /mnt --dir <tmp> and refresh hosts/hardware/{host}.nix under {})",
            staged_flake.display()
        );
        return Ok(());
    }
    let hw_path = hardware_file_path(staged_flake, host)?;

    // Optional: back up the original for debugging, kept OUTSIDE the flake copy
    // so it can never be picked up by auto-import or land in the store path.
    if hw_path.exists() {
        let bak = PathBuf::from(format!("{}.install-nix-bak", hw_path.display()));
        if let Some(parent) = bak.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::copy(&hw_path, &bak).ok();
    }

    let gen_dir = tempfile::Builder::new()
        .prefix("install-nix-hw-")
        .tempdir()
        .context("failed to create temp dir for hardware config")?;
    let gen_file = gen_dir.path().join("hardware-configuration.nix");

    cmd::run(
        ctx,
        "nixos-generate-config",
        &[
            "--root",
            "/mnt",
            "--dir",
            &format!("{}", gen_dir.path().display()),
        ],
    )
    .ok();

    if gen_file.is_file() {
        let content = std::fs::read_to_string(&gen_file)
            .with_context(|| format!("generated config missing at {}", gen_file.display()))?;
        let header = format!(
            "# Generated by install-nix for host `{host}` (fresh install).\n\
             # Source: nixos-generate-config --root /mnt on the live ISO.\n\
             # Do not hand-edit: regenerate with `sudo nixos-generate-config`.\n"
        );
        let merged = if content.starts_with("# Do not modify") {
            // Keep upstream's warning, add our provenance line after it.
            let mut lines = content.lines();
            let mut out = String::new();
            for _ in 0..2 {
                if let Some(l) = lines.next() {
                    out.push_str(l);
                    out.push('\n');
                }
            }
            out.push_str(&header);
            for l in lines {
                out.push_str(l);
                out.push('\n');
            }
            out
        } else {
            format!("{header}{content}")
        };
        atomic_write(&hw_path, &merged)?;
        println!("hardware config: refreshed {}", hw_path.display());
        return Ok(());
    }

    // Fallback: synthesize from blkid UUIDs.
    eprintln!("warning: nixos-generate-config failed; synthesizing hardware file from blkid");
    synthesize_hardware_file(host, layout, &hw_path)?;
    println!("hardware config: synthesized {}", hw_path.display());
    Ok(())
}

/// Blkid fallback for when nixos-generate-config is unavailable.
pub fn synthesize_hardware_file(host: &str, layout: &disk::Layout, hw_path: &Path) -> Result<()> {
    let root_uuid = disk::uuid_of(&layout.root)?;
    let esp_uuid = disk::uuid_of(&layout.esp)?;
    let swap_uuid = layout.swap.as_deref().map(disk::uuid_of).transpose()?;
    let swap_block = match swap_uuid {
        Some(u) => {
            format!("  swapDevices = [ {{ device = \"/dev/disk/by-uuid/{u}\"; }} ];\n")
        }
        None => "  swapDevices = [ ];\n".to_string(),
    };
    // Platform comes from the running (live) machine so ARM installers don't
    // get an x86_64 hostPlatform.
    let platform = live_platform();
    let content = format!(
        "# Generated by install-nix fallback (blkid) for host `{host}`.\n\
         # Regenerate authoritatively with: sudo nixos-generate-config --show-hardware-config\n\
         {{ config, lib, pkgs, modulesPath, ... }}:\n\
         {{\n\
           imports = [ (modulesPath + \"/installer/scan/not-detected.nix\") ];\n\
         \n\
           boot.initrd.availableKernelModules = [ \"nvme\" \"xhci_pci\" \"ahci\" \"usbhid\" \"usb_storage\" \"sd_mod\" ];\n\
           boot.initrd.kernelModules = [ ];\n\
           boot.kernelModules = [ ];\n\
           boot.extraModulePackages = [ ];\n\
         \n\
           fileSystems.\"/\" = {{ device = \"/dev/disk/by-uuid/{root_uuid}\"; fsType = \"ext4\"; }};\n\
           fileSystems.\"/boot\" = {{ device = \"/dev/disk/by-uuid/{esp_uuid}\"; fsType = \"vfat\"; options = [ \"fmask=0077\" \"dmask=0077\" ]; }};\n\
         \n\
         {swap_block}\n\
           networking.useDHCP = lib.mkDefault true;\n\
           nixpkgs.hostPlatform = lib.mkDefault \"{platform}\";\n\
         }}\n"
    );
    atomic_write(hw_path, &content)?;
    Ok(())
}

/// Write via temp file + rename so a crash can never leave a half-written
/// hardware config that would be evaluated by nixos-install.
fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let dir = path.parent().context("hardware file has no parent dir")?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".install-nix-hw-")
        .tempfile_in(dir)
        .context("failed to create temporary hardware file")?;
    std::io::Write::write_all(&mut tmp, content.as_bytes())
        .context("failed to write temporary hardware file")?;
    tmp.flush().ok();
    tmp.persist(path).map_err(|e| {
        anyhow!(
            "failed to move generated hardware file into place: {}",
            e.error
        )
    })?;
    Ok(())
}

/// Nix hostPlatform for the machine running install-nix (`uname -m`).
fn live_platform() -> String {
    match cmd::run_capture("uname", &["-m"]) {
        Ok(m) => match m.trim() {
            "x86_64" => "x86_64-linux".to_string(),
            "aarch64" | "arm64" => "aarch64-linux".to_string(),
            "riscv64" => "riscv64-linux".to_string(),
            other => format!("{other}-linux"),
        },
        Err(_) => "x86_64-linux".to_string(),
    }
}

/// Validate that the flake defines hosts/<host>.nix.
pub fn check_host_exists(staged_flake: &Path, host: &str) -> Result<()> {
    valid_host_name(host)?;
    if staged_flake.join(format!("hosts/{host}.nix")).is_file() {
        return Ok(());
    }
    let avail = available_hosts(staged_flake);
    if avail.is_empty() {
        return Err(anyhow!(
            "host `{host}` not found (no hosts/*.nix in {})",
            staged_flake.display()
        ));
    }
    Err(anyhow!(
        "host `{host}` not found in {}. Available: {}",
        staged_flake.display(),
        avail.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::Ctx;

    #[test]
    fn host_name_accepts_normal() {
        for h in ["desktop", "laptop", "live-desktop", "a", "host_9"] {
            assert!(valid_host_name(h).is_ok(), "{h} should be valid");
        }
    }

    #[test]
    fn host_name_rejects_traversal() {
        for h in [
            "",
            "..",
            ".",
            "/etc/passwd",
            "../evil",
            "host/../../etc/x",
            "host\nname",
            "host name",
            ".hidden",
            "foo/bar",
            "UPPER",
            "-leading-dash",
            &"x".repeat(65),
        ] {
            assert!(valid_host_name(h).is_err(), "{h:?} should be rejected");
        }
    }

    /// Build a minimal fake dotfiles tree: flake.nix + hosts/<host>.nix +
    /// hosts/hardware/<host>.nix, optionally with a .git dir.
    fn fixture(with_git: bool) -> tempfile::TempDir {
        let tmp = tempfile::Builder::new()
            .prefix("install-nix-test-")
            .tempdir()
            .unwrap();
        std::fs::write(tmp.path().join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("hosts/hardware")).unwrap();
        std::fs::write(tmp.path().join("hosts/desktop.nix"), "{}\n").unwrap();
        std::fs::write(tmp.path().join("hosts/hardware/desktop.nix"), "{}\n").unwrap();
        if with_git {
            std::fs::create_dir_all(tmp.path().join(".git/objects")).unwrap();
        }
        tmp
    }

    #[test]
    fn staging_detaches_git() {
        let src = fixture(true);
        let ctx = Ctx::new(false);
        let (_tmp, dst) = stage_writable_copy(ctx, src.path()).unwrap();
        assert!(dst.join("flake.nix").is_file());
        assert!(dst.join("hosts/hardware/desktop.nix").is_file());
        assert!(
            !dst.join(".git").exists(),
            ".git must be moved aside so untracked files stay visible"
        );
        assert!(dst.join(".git.install-nix-bak").is_dir());
    }

    #[test]
    fn hardware_path_refuses_symlink_target() {
        use std::os::unix::fs::symlink;
        let src = fixture(false);
        let target = src.path().join("hosts/hardware/desktop.nix");
        std::fs::remove_file(&target).unwrap();
        symlink("/etc/hostname", &target).unwrap();
        assert!(hardware_file_path(src.path(), "desktop").is_err());
    }

    #[test]
    fn check_host_exists_uses_allow_list() {
        let src = fixture(false);
        assert!(check_host_exists(src.path(), "desktop").is_ok());
        // live-*/common are filtered from installable hosts...
        assert!(check_host_exists(src.path(), "live-desktop").is_err());
        // ...and traversal is rejected even though the joined path differs.
        assert!(check_host_exists(src.path(), "../flake").is_err());
    }
}
