use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::cmd::{self, Ctx};

/// One whole-disk block device from `lsblk -J`.
#[derive(Debug, Clone)]
pub struct Disk {
    /// e.g. /dev/nvme0n1
    pub path: String,
    /// Human size from lsblk, e.g. "953.9G"
    pub size_h: String,
    /// Size in bytes (parsed from lsblk SIZE? we request BYTES via --bytes).
    pub size_bytes: u64,
    pub model: String,
    /// Transport: nvme, sata, usb, ...
    pub tran: String,
    /// Removable flag ("1" for USB sticks etc.), if reported.
    pub rm: String,
    /// Child mountpoints (partitions already mounted), for warnings.
    pub mounts: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Lsblk {
    blockdevices: Vec<LsblkDev>,
}

#[derive(Debug, Deserialize)]
struct LsblkDev {
    name: Option<String>,
    path: Option<String>,
    size: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tran: Option<String>,
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    rm: Option<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    ro: Option<serde_json::Value>,
    #[serde(default)]
    mountpoint: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    mountpoints: Option<serde_json::Value>,
    #[serde(default)]
    fstype: Option<String>,
    #[serde(default)]
    children: Option<Vec<LsblkDev>>,
}

/// One partition (or whole-disk filesystem) on the target disk.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Part {
    path: String,
    fstype: String,
    mountpoint: String,
}

/// Partition layout computed from user options.
#[derive(Debug, Clone)]
pub struct Layout {
    pub esp: String,
    pub swap: Option<String>,
    pub root: String,
}

/// Partition-size policy (see `validate_sizes`).
pub const MIN_EFI_MIB: u64 = 128;
pub const MAX_EFI_MIB: u64 = 4096;
pub const MAX_SWAP_GIB: u64 = 64;
/// Minimum ext4 root we consider a viable NixOS install.
pub const MIN_ROOT_MIB: u64 = 8 * 1024;
/// GPT layout starts the ESP at 1MiB.
pub const GPT_LEAD_MIB: u64 = 1;

/// `nvme0n1 -> /dev/nvme0n1p1`, `sda -> /dev/sda1`.
pub fn partition_path(disk: &str, n: u32) -> String {
    let last = disk.chars().last().unwrap_or('x');
    if last.is_ascii_digit() {
        format!("{disk}p{n}")
    } else {
        format!("{disk}{n}")
    }
}

/// List whole disks via lsblk. Never fails hard on one bad entry.
pub fn list_disks() -> Result<Vec<Disk>> {
    let out = cmd::run_capture(
        "lsblk",
        &[
            "-J",
            "--bytes",
            "-o",
            "NAME,PATH,SIZE,MODEL,TRAN,TYPE,RM,RO,MOUNTPOINT",
        ],
    )
    .context("lsblk failed; is util-linux installed?")?;
    let parsed: Lsblk = serde_json::from_str(&out).context("failed to parse `lsblk -J` output")?;
    let mut disks = Vec::new();
    for d in &parsed.blockdevices {
        if d.r#type.as_deref() != Some("disk") {
            continue;
        }
        let name = d.name.clone().unwrap_or_default();
        // Skip loop/ram/zram explicitly even if TYPE says disk.
        if name.starts_with("loop")
            || name.starts_with("ram")
            || name.starts_with("zram")
            || name.starts_with("dm-")
        {
            continue;
        }
        let path = d.path.clone().unwrap_or_else(|| format!("/dev/{name}"));
        let size_h = d.size.clone().unwrap_or_else(|| "?".into());
        let size_bytes = size_h.parse::<u64>().unwrap_or(0);
        let mut mounts = Vec::new();
        collect_mounts(d, &mut mounts);
        disks.push(Disk {
            path,
            size_h: humansize(size_bytes, &size_h),
            size_bytes,
            model: d.model.clone().unwrap_or_default().trim().to_string(),
            tran: d.tran.clone().unwrap_or_default().trim().to_string(),
            rm: json_flag(&d.rm),
            mounts,
        });
    }
    disks.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(disks)
}

fn collect_mounts(dev: &LsblkDev, out: &mut Vec<String>) {
    if let Some(mp) = &dev.mountpoint {
        if !mp.is_empty() && mp != "null" {
            out.push(mp.clone());
        }
    }
    if let Some(children) = &dev.children {
        for c in children {
            collect_mounts(c, out);
        }
    }
}

fn json_flag(v: &Option<serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::Bool(b)) => {
            if *b {
                "1".into()
            } else {
                "0".into()
            }
        }
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn humansize(bytes: u64, fallback: &str) -> String {
    if bytes == 0 {
        return fallback.to_string();
    }
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes}B")
    } else {
        format!("{v:.1}{}", UNITS[u])
    }
}

/// Read-only probe: is `target` currently a mountpoint?
fn is_mounted(target: &str) -> bool {
    cmd::run_capture("findmnt", &["-n", target]).is_ok()
}

/// Read-only probe: is `dev` currently active swap?
fn swap_is_active(dev: &str) -> bool {
    cmd::run_capture("swapon", &["--show=NAME", "--noheadings"])
        .map(|o| o.lines().any(|l| l.trim() == dev))
        .unwrap_or(false)
}

/// Which whole disk backs the live ISO, if any.
///
/// Fail-closed: when `/iso` is mounted we MUST resolve its backing disk;
/// any resolution failure is an error (never `None`), so callers cannot
/// mistake "detection broke" for "no live media present". When `/iso` is
/// not mounted at all (dev machine, installed system) there is no live
/// media to protect and this returns `Ok(None)`.
pub fn live_source_disk() -> Result<Option<String>> {
    if !is_mounted("/iso") {
        return Ok(None);
    }
    let src = cmd::run_capture("findmnt", &["-n", "-o", "SOURCE", "/iso"])
        .context("cannot determine /iso source device")?;
    let src = src.trim();
    if src.is_empty() {
        bail!("cannot determine /iso source device (empty SOURCE)");
    }
    // Optical media: nothing else to protect beyond itself.
    if src.starts_with("/dev/sr") {
        return Ok(Some(src.to_string()));
    }
    // Resolve LABEL=/UUID= textual sources to device nodes.
    let dev = if let Some(label) = src.strip_prefix("LABEL=") {
        cmd::run_capture("blkid", &["-L", label]).context("cannot resolve /iso LABEL")?
    } else if let Some(uuid) = src.strip_prefix("UUID=") {
        cmd::run_capture("blkid", &["-U", uuid]).context("cannot resolve /iso UUID")?
    } else {
        src.to_string()
    };
    let dev = dev.trim().to_string();
    // Partition -> parent disk via the kernel's own parent pointer.
    let pk = cmd::run_capture("lsblk", &["-n", "-o", "PKNAME", &dev]).unwrap_or_default();
    let parent = resolve_live_device(&dev, pk.trim());
    if parent.contains("/disk/") {
        bail!("cannot map /iso source {dev:?} to a parent disk; refusing to guess");
    }
    Ok(Some(parent))
}

/// Pure parent-disk mapping, split out for testing:
/// PKNAME wins; else strip a partition suffix; else the device itself.
fn resolve_live_device(dev: &str, pkname: &str) -> String {
    if !pkname.is_empty() {
        return format!("/dev/{pkname}");
    }
    if let Some(parent) = strip_partition_suffix(dev) {
        return parent;
    }
    dev.to_string()
}

/// Strip a trailing partition number: /dev/sda1 -> /dev/sda,
/// /dev/nvme0n1p2 -> /dev/nvme0n1. Returns None if it looks like a whole disk.
fn strip_partition_suffix(dev: &str) -> Option<String> {
    // nvme / mmcblk: ...n1p2
    if let Some(idx) = dev.rfind('p') {
        let (disk, part) = dev.split_at(idx);
        if (disk.contains("nvme") || disk.contains("mmcblk"))
            && !part[1..].is_empty()
            && part[1..].chars().all(|c| c.is_ascii_digit())
            && disk
                .chars()
                .last()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
        {
            return Some(disk.to_string());
        }
    }
    // sda1 / vda12 / hda1: trailing digits after a non-digit.
    let trim: String = dev
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .to_string();
    if trim.len() < dev.len()
        && !trim.is_empty()
        && !trim.ends_with('/')
        && trim.starts_with("/dev/")
        && !trim
            .chars()
            .last()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(true)
    {
        // Guard: /dev/nvme0n1 ends in a digit but is a whole disk (no 'p').
        if trim.contains("nvme") || trim.contains("mmcblk") {
            return None;
        }
        return Some(trim);
    }
    None
}

/// Refuse obviously wrong targets before we touch anything.
pub fn validate_target(disk: &Disk, live: Option<&str>) -> Result<()> {
    if let Some(live_dev) = live {
        if disk.path == live_dev {
            bail!(
                "refusing: {} is the device backing the live ISO (mounted at /iso). Pick another drive.",
                disk.path
            );
        }
    }
    Ok(())
}

/// Validate EFI/swap sizes and disk capacity BEFORE anything destructive.
///
/// All inputs are bounded by the constants above, so the MiB arithmetic
/// cannot overflow (max: 1 + 4096 + 64*1024 + 8192 MiB).
pub fn validate_sizes(efi_mib: u64, swap_gib: u64, disk_bytes: u64, disk_path: &str) -> Result<()> {
    if !(MIN_EFI_MIB..=MAX_EFI_MIB).contains(&efi_mib) {
        bail!("--efi-size {efi_mib} out of range ({MIN_EFI_MIB}–{MAX_EFI_MIB} MiB)");
    }
    if swap_gib > MAX_SWAP_GIB {
        bail!("--swap-size {swap_gib} out of range (0–{MAX_SWAP_GIB} GiB)");
    }
    if disk_bytes == 0 {
        eprintln!("warning: cannot read size of {disk_path}; skipping capacity check");
        return Ok(());
    }
    let need_mib = GPT_LEAD_MIB + efi_mib + swap_gib * 1024 + MIN_ROOT_MIB;
    let need_bytes = need_mib * 1024 * 1024;
    if disk_bytes < need_bytes {
        bail!(
            "{disk_path} is too small: layout needs {need_mib} MiB (1 lead + EFI {efi_mib} + swap {} + root {MIN_ROOT_MIB} minimum), disk has {}",
            swap_gib * 1024,
            humansize(disk_bytes, "?"),
        );
    }
    Ok(())
}

/// Refuse to proceed when /mnt or /mnt/boot already hosts mounts.
/// Our install owns /mnt exclusively; mounting over (or recursively
/// unmounting) someone else's mounts destroys data or breaks the host.
pub fn require_mnt_free() -> Result<()> {
    for t in ["/mnt", "/mnt/boot"] {
        if is_mounted(t) {
            let src = cmd::run_capture("findmnt", &["-n", "-o", "SOURCE", t]).unwrap_or_default();
            bail!(
                "refusing: {t} already has {} mounted; unmount it first",
                src.trim()
            );
        }
    }
    Ok(())
}

/// All partitions (children) of `drive`, plus a whole-disk filesystem
/// if the drive node itself carries one.
fn disk_partitions(drive: &str) -> Result<Vec<Part>> {
    let out = cmd::run_capture("lsblk", &["-J", "-o", "PATH,FSTYPE,MOUNTPOINT", drive])?;
    let parsed: Lsblk = serde_json::from_str(&out).context("failed to parse `lsblk -J` output")?;
    let mut parts = Vec::new();
    let push = |d: &LsblkDev, parts: &mut Vec<Part>| {
        let path = d.path.clone().unwrap_or_default();
        if path.is_empty() {
            return;
        }
        let mountpoint = match d.mountpoint.as_deref().unwrap_or("") {
            "" | "null" => String::new(),
            mp => mp.to_string(),
        };
        parts.push(Part {
            path,
            fstype: d.fstype.clone().unwrap_or_default(),
            mountpoint,
        });
    };
    for d in &parsed.blockdevices {
        // Whole-disk filesystem directly on the drive node (no partition table).
        if d.children.is_none() {
            push(d, &mut parts);
        } else if let Some(children) = &d.children {
            for c in children {
                push(c, &mut parts);
            }
        }
    }
    Ok(parts)
}

/// Unmount every mounted partition of the target disk and deactivate its
/// swap partitions — after user confirmation, before wipefs.
/// Only this disk's partitions are touched. Fails hard if anything
/// remains mounted: wiping a mounted filesystem is never acceptable.
pub fn unmount_disk(ctx: Ctx, drive: &str) -> Result<()> {
    let parts = disk_partitions(drive)?;
    for p in &parts {
        if !p.mountpoint.is_empty() {
            cmd::run(ctx, "umount", &[&p.path])?;
        }
    }
    for p in &parts {
        if p.fstype == "swap" && (ctx.dry_run || swap_is_active(&p.path)) {
            cmd::run(ctx, "swapoff", &[&p.path])?;
        }
    }
    if ctx.dry_run {
        println!("(dry-run: would verify nothing on {drive} is still mounted)");
        return Ok(());
    }
    // Re-verify: refuse to wipe while anything is still mounted.
    for p in disk_partitions(drive)? {
        if !p.mountpoint.is_empty() {
            bail!(
                "{} is still mounted at {}; refusing to wipe {drive}",
                p.path,
                p.mountpoint
            );
        }
    }
    Ok(())
}

/// Wipe, partition (GPT: ESP + optional swap + root), format, and mount at /mnt.
pub fn partition_format_mount(
    ctx: Ctx,
    drive: &str,
    efi_mib: u64,
    swap_gib: u64,
) -> Result<Layout> {
    // Sanity: must be a whole disk, not a partition.
    let dtype = cmd::run_capture("lsblk", &["-n", "-o", "TYPE", drive])
        .with_context(|| format!("cannot probe {drive}; does it exist?"))?;
    let dtype = dtype.trim();
    if dtype != "disk" {
        bail!("{drive} is TYPE={dtype:?}, not a whole disk — pass the disk (e.g. /dev/sda), not a partition");
    }

    let swap_mib = swap_gib * 1024;
    let esp_end = 1 + efi_mib;
    let (swap_end, root_start) = if swap_mib > 0 {
        (esp_end + swap_mib, esp_end + swap_mib)
    } else {
        (esp_end, esp_end)
    };

    let esp = partition_path(drive, 1);
    let (swap, root) = if swap_mib > 0 {
        (Some(partition_path(drive, 2)), partition_path(drive, 3))
    } else {
        (None, partition_path(drive, 2))
    };

    // 1. Wipe existing signatures (so stale RAID/LVM metadata can't confuse parted).
    cmd::run(ctx, "wipefs", &["-a", drive])?;

    // 2. Fresh GPT + partitions.
    cmd::run(ctx, "parted", &["-s", drive, "mklabel", "gpt"])?;
    cmd::run(
        ctx,
        "parted",
        &[
            "-s",
            drive,
            "mkpart",
            "ESP",
            "fat32",
            "1MiB",
            &format!("{esp_end}MiB"),
        ],
    )?;
    cmd::run(ctx, "parted", &["-s", drive, "set", "1", "esp", "on"])?;
    if swap_mib > 0 {
        cmd::run(
            ctx,
            "parted",
            &[
                "-s",
                drive,
                "mkpart",
                "swap",
                "linux-swap",
                &format!("{esp_end}MiB"),
                &format!("{swap_end}MiB"),
            ],
        )?;
        cmd::run(
            ctx,
            "parted",
            &[
                "-s",
                drive,
                "mkpart",
                "root",
                "ext4",
                &format!("{root_start}MiB"),
                "100%",
            ],
        )?;
    } else {
        cmd::run(
            ctx,
            "parted",
            &[
                "-s",
                drive,
                "mkpart",
                "root",
                "ext4",
                &format!("{root_start}MiB"),
                "100%",
            ],
        )?;
    }

    // 3. Let the kernel catch up before formatting. ctx-aware: skipped in dry-run.
    cmd::run(ctx, "udevadm", &["settle"])?;
    cmd::run(ctx, "partprobe", &[drive])?;

    // 4. Format.
    cmd::run(ctx, "mkfs.fat", &["-F", "32", "-n", "EFI", &esp])?;
    if let Some(ref sp) = swap {
        cmd::run(ctx, "mkswap", &["-L", "swap", sp])?;
    }
    cmd::run(ctx, "mkfs.ext4", &["-F", "-L", "nixos", &root])?;

    // 5. Mount at /mnt (/mnt was verified free before the wipe).
    cmd::run(ctx, "mount", &[&root, "/mnt"])?;
    cmd::run(ctx, "mkdir", &["-p", "/mnt/boot"])?;
    cmd::run(ctx, "mount", &["-o", "umask=077", &esp, "/mnt/boot"])?;
    if let Some(ref sp) = swap {
        // swapon failure shouldn't abort the install; the swap line in the
        // generated hardware config still lets the installed system use it.
        if let Err(e) = cmd::run(ctx, "swapon", &[sp]) {
            eprintln!("warning: swapon failed ({e}); continuing without active swap");
        }
    }

    Ok(Layout { esp, swap, root })
}

/// Ordered leaf-first unmount of exactly the mounts we created.
/// Never `umount -R`: that could detach mounts we don't own.
pub fn planned_unmounts() -> [&'static str; 2] {
    ["/mnt/boot", "/mnt"]
}

/// Undo our mounts after install: our swap partition only (never `swapoff -a`,
/// which would drop unrelated active swap), then /mnt/boot, then /mnt.
/// Targets that are already unmounted/inactive are skipped via read-only probes.
pub fn unmount_all(ctx: Ctx, swap: Option<&str>) -> Result<()> {
    if let Some(sp) = swap {
        if ctx.dry_run || swap_is_active(sp) {
            cmd::run(ctx, "swapoff", &[sp])?;
        }
    }
    for t in planned_unmounts() {
        if ctx.dry_run || is_mounted(t) {
            cmd::run(ctx, "umount", &[t])?;
        }
    }
    Ok(())
}

/// UUID of a freshly formatted partition (blkid).
pub fn uuid_of(part: &str) -> Result<String> {
    let u = cmd::run_capture("blkid", &["-s", "UUID", "-o", "value", part])?;
    let u = u.trim().to_string();
    if u.is_empty() {
        return Err(anyhow!("blkid returned no UUID for {part}"));
    }
    Ok(u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_naming_sata() {
        assert_eq!(partition_path("/dev/sda", 1), "/dev/sda1");
        assert_eq!(partition_path("/dev/vda", 3), "/dev/vda3");
    }

    #[test]
    fn partition_naming_nvme() {
        assert_eq!(partition_path("/dev/nvme0n1", 1), "/dev/nvme0n1p1");
        assert_eq!(partition_path("/dev/mmcblk0", 2), "/dev/mmcblk0p2");
    }

    #[test]
    fn strip_suffix_cases() {
        assert_eq!(strip_partition_suffix("/dev/sda1"), Some("/dev/sda".into()));
        assert_eq!(
            strip_partition_suffix("/dev/nvme0n1p2"),
            Some("/dev/nvme0n1".into())
        );
        assert_eq!(strip_partition_suffix("/dev/sda"), None);
        assert_eq!(strip_partition_suffix("/dev/nvme0n1"), None);
    }

    #[test]
    fn live_device_prefers_pkname() {
        // Kernel parent pointer wins over suffix heuristics.
        assert_eq!(resolve_live_device("/dev/sda1", "sda"), "/dev/sda");
        assert_eq!(
            resolve_live_device("/dev/nvme0n1p2", "nvme0n1"),
            "/dev/nvme0n1"
        );
    }

    #[test]
    fn live_device_falls_back_to_suffix_strip() {
        // No PKNAME (older lsblk): SATA suffix still maps to the parent disk.
        assert_eq!(resolve_live_device("/dev/sdb1", ""), "/dev/sdb");
        assert_eq!(resolve_live_device("/dev/nvme0n1p1", ""), "/dev/nvme0n1");
    }

    #[test]
    fn live_device_whole_disk_passes_through() {
        assert_eq!(resolve_live_device("/dev/sda", ""), "/dev/sda");
        assert_eq!(resolve_live_device("/dev/nvme0n1", ""), "/dev/nvme0n1");
    }

    #[test]
    fn sizes_accept_defaults_on_roomy_disk() {
        let sixty_four_gib = 64 * 1024 * 1024 * 1024;
        assert!(validate_sizes(512, 8, sixty_four_gib, "/dev/sda").is_ok());
        assert!(validate_sizes(128, 0, sixty_four_gib, "/dev/sda").is_ok());
    }

    #[test]
    fn sizes_reject_out_of_range() {
        let big = 128 * 1024 * 1024 * 1024;
        assert!(validate_sizes(64, 8, big, "/dev/sda").is_err()); // EFI too small
        assert!(validate_sizes(8192, 8, big, "/dev/sda").is_err()); // EFI too large
        assert!(validate_sizes(512, 65, big, "/dev/sda").is_err()); // swap too large
    }

    #[test]
    fn sizes_reject_small_disk() {
        // Defaults need 1 + 512 + 8192 + 8192 = 16897 MiB.
        let eight_gib = 8 * 1024 * 1024 * 1024;
        assert!(validate_sizes(512, 8, eight_gib, "/dev/sda").is_err());
        // ...but a swapless small layout fits a 10 GiB disk.
        let ten_gib = 10 * 1024 * 1024 * 1024;
        assert!(validate_sizes(512, 0, ten_gib, "/dev/sda").is_ok());
    }

    #[test]
    fn sizes_unknown_size_warns_but_passes() {
        assert!(validate_sizes(512, 8, 0, "/dev/sda").is_ok());
    }

    #[test]
    fn unmount_order_is_leaf_first() {
        assert_eq!(planned_unmounts(), ["/mnt/boot", "/mnt"]);
    }
}
