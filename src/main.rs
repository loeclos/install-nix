mod cli;
mod cmd;
mod disk;
mod flake;

use anyhow::{bail, Context, Result};
use clap::Parser;
use dialoguer::{Confirm, Input, Password, Select};
use std::path::Path;

use cli::Args;
use cmd::Ctx;

const REQUIRED_TOOLS: &[&str] = &[
    "parted",
    "wipefs",
    "mkfs.fat",
    "mkfs.ext4",
    "mkswap",
    "swapon",
    "swapoff",
    "blkid",
    "lsblk",
    "findmnt",
    "mount",
    "mkdir",
    "umount",
    "cp",
    "chmod",
    "partprobe",
    "udevadm",
    "uname",
    "nixos-install",
    "nixos-generate-config",
    "nixos-enter",
];

/// nixos-install flags that would redirect the install target, swap the
/// evaluated config, or skip boot-critical steps. install-nix owns --flake,
/// --root and --no-root-passwd; everything here is rejected when forwarded
/// after `--` (build/display options like --impure, --show-trace, --option,
/// --max-jobs, --cores, --no-channel-copy, -v stay allowed).
const MANAGED_INSTALL_FLAGS: &[&str] = &[
    "flake",
    "file",
    "attr",
    "root",
    "system",
    "closure",
    "store-path",
    "channel",
    "no-root-passwd",
    "no-root-password",
    "no-bootloader",
];

/// True when a forwarded nixos-install arg must be refused.
/// Handles `--flag value`, `--flag=value`, Getopt-style unambiguous
/// abbreviations (`--fl` for `--flake`), and short/bundled forms
/// (`-f`, `-A`, `-vf`).
fn is_forbidden_install_arg(arg: &str) -> bool {
    if let Some(long) = arg.strip_prefix("--") {
        let name = long.split('=').next().unwrap_or("");
        if name.is_empty() {
            return false; // bare "--" separator; nixos-install handles it.
        }
        return MANAGED_INSTALL_FLAGS
            .iter()
            .any(|m| *m == name || m.starts_with(name) || name.starts_with(m));
    }
    if let Some(short) = arg.strip_prefix('-') {
        // -f/--file and -A/--attr take alternative config sources;
        // refuse them and any bundling containing them (-vf, -Af, ...).
        return short.contains('f') || short.contains('A');
    }
    false
}

/// Assemble the final nixos-install argv: user extras first, then the
/// enforced flags LAST so nothing forwarded can override --flake/--root
/// even under last-wins option parsing.
pub fn build_install_args(staged: &Path, host: &str, extra: &[String]) -> Result<Vec<String>> {
    for a in extra {
        if is_forbidden_install_arg(a) {
            bail!(
                "{a:?} is managed by install-nix (install target is fixed); refusing forwarded nixos-install args that redirect the install"
            );
        }
    }
    let mut argv: Vec<String> = extra.to_vec();
    argv.extend([
        "--flake".to_string(),
        format!("{}#{host}", staged.display()),
        "--root".to_string(),
        "/mnt".to_string(),
        "--no-root-passwd".to_string(),
    ]);
    Ok(argv)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let ctx = Ctx::new(args.dry_run);

    if args.dry_run {
        println!("=== DRY RUN: read-only probes run; nothing is created, mounted, or wiped ===");
    }

    require_root()?;
    check_tools(args.reboot)?;

    // 1. Flake source.
    let flake_src = flake::resolve_flake(args.flake.as_deref())?;
    println!("flake source: {}", flake_src.display());

    // 2. Target host — validated against the SOURCE flake before anything
    //    destructive, so a typo can never cost a wiped disk.
    let host = resolve_host(&args, &flake_src)?;
    flake::check_host_exists(&flake_src, &host)?;
    println!("install host: {host}");

    // 3. Target drive. Live-media detection is fail-closed: when /iso is
    //    mounted its backing disk MUST resolve, or we abort.
    let drive = resolve_drive(&args)?;
    let live = disk::live_source_disk()?;
    match &live {
        Some(l) => println!("live ISO device: {l} (refusing it as a target)"),
        None => println!("live ISO device: none detected (/iso not mounted)"),
    }
    let disk = lookup_disk(&drive)?;
    disk::validate_target(&disk, live.as_deref())?;
    disk::validate_sizes(args.efi_size, args.swap_size, disk.size_bytes, &disk.path)?;
    if disk.size_bytes > 0 && disk.size_bytes < 15 * 1024 * 1024 * 1024 {
        eprintln!(
            "warning: {} is only {} — 20G+ recommended for NixOS + home-manager",
            disk.path, disk.size_h
        );
    }
    if !disk.mounts.is_empty() {
        eprintln!(
            "note: {} has mounted partitions ({}); they will be unmounted after confirmation, then the disk wiped",
            disk.path,
            disk.mounts.join(", ")
        );
    }
    println!(
        "target drive: {} ({} model:{} tran:{})",
        disk.path,
        disk.size_h,
        if disk.model.is_empty() {
            "?"
        } else {
            &disk.model
        },
        if disk.tran.is_empty() {
            "?"
        } else {
            &disk.tran
        }
    );

    // 4. /mnt must be free BEFORE the wipe — the install owns it exclusively.
    disk::require_mnt_free()?;

    // 5. Confirm wipe.
    confirm_wipe(&args, &disk.path)?;

    // 6. Unmount the target's own partitions/swap (post-confirmation only),
    //    then wipe, partition, format, and mount at /mnt.
    disk::unmount_disk(ctx, &drive)?;
    println!(
        "\n--- partitioning {drive} (GPT: EFI {}MiB + {} + ext4 root) ---",
        args.efi_size,
        swap_label(args.swap_size)
    );
    let layout = disk::partition_format_mount(ctx, &drive, args.efi_size, args.swap_size)?;
    println!(
        "mounted: {} -> /mnt, {} -> /mnt/boot{}",
        layout.root,
        layout.esp,
        layout
            .swap
            .as_deref()
            .map(|s| format!(", swap {s}"))
            .unwrap_or_default()
    );

    // 7. Stage writable flake copy + refresh hardware config.
    //    The host is re-checked against the staged copy (defense in depth).
    println!("\n--- staging flake ---");
    let (_tmp, staged) = flake::stage_writable_copy(ctx, &flake_src)?;
    if !ctx.dry_run {
        flake::check_host_exists(&staged, &host)?;
    }
    flake::refresh_hardware_config(ctx, &staged, &host, &layout)?;

    // 8. nixos-install with enforced --flake/--root.
    println!(
        "\n--- nixos-install --flake {staged}#{host} ---",
        staged = staged.display()
    );
    let install = build_install_args(&staged, &host, &args.install_args)?;
    cmd::run(ctx, "nixos-install", &install)
        .context("nixos-install failed — /mnt is left mounted for inspection")?;

    // 9. User password (root stays locked via --no-root-passwd).
    if args.no_set_password {
        println!("skipping password setup (--no-set-password)");
    } else if ctx.dry_run {
        println!("(dry-run: would prompt for loeclos password + chpasswd via nixos-enter)");
    } else {
        set_user_password()?;
    }

    // 10. Unmount / reboot. A reboot is only issued after a SUCCESSFUL
    //     unmount — never after a failed one.
    if args.reboot {
        if ctx.dry_run {
            println!("\n(dry-run: would unmount /mnt, then reboot)");
        } else {
            println!("\n--- unmounting /mnt ---");
            disk::unmount_all(ctx, layout.swap.as_deref())?;
            println!("--- rebooting ---");
            cmd::run(ctx, "reboot", &[] as &[String])?;
        }
    } else if args.no_unmount {
        println!("\nleaving /mnt mounted (--no-unmount)");
    } else {
        println!("\n--- unmounting /mnt ---");
        disk::unmount_all(ctx, layout.swap.as_deref())?;
    }

    println!("\ndone: {drive} now boots {host}. Reboot and remove the installer ISO.");
    Ok(())
}

fn swap_label(gib: u64) -> String {
    if gib == 0 {
        "no swap".into()
    } else {
        format!("swap {gib}GiB")
    }
}

fn require_root() -> Result<()> {
    // `id -u` is universal (no libc dep). Runs even in dry-run (read-only).
    let uid = cmd::run_capture("id", &["-u"])?;
    if uid.trim() != "0" {
        bail!("must run as root (try: sudo install-nix)");
    }
    Ok(())
}

fn check_tools(want_reboot: bool) -> Result<()> {
    let mut missing: Vec<_> = REQUIRED_TOOLS.iter().filter(|t| !cmd::have(t)).collect();
    if want_reboot && !cmd::have("reboot") {
        missing.push(&"reboot");
    }
    if !missing.is_empty() {
        let list: Vec<String> = missing.iter().map(|s| s.to_string()).collect();
        bail!(
            "missing required tools: {}. On the live ISO these all ship with hosts/live.nix; run from the ISO",
            list.join(", ")
        );
    }
    Ok(())
}

fn resolve_host(args: &Args, flake_src: &std::path::Path) -> Result<String> {
    if let Some(h) = &args.host {
        let h = h.trim().to_string();
        flake::valid_host_name(&h)?;
        return Ok(h);
    }
    let available = flake::available_hosts(flake_src);
    let guess = flake::guess_host_from_live();
    // Prefer interactive selection when on a TTY; fall back to guess/only-host.
    if console_is_tty() && !available.is_empty() {
        let mut items: Vec<String> = available.clone();
        let mut preselect = 0;
        if let Some(ref g) = guess {
            if flake::valid_host_name(g).is_ok() {
                if let Some(i) = items.iter().position(|h| h == g) {
                    preselect = i;
                } else {
                    items.push(g.clone());
                    preselect = items.len() - 1;
                }
            }
        }
        let labels: Vec<String> = items
            .iter()
            .map(|h| {
                if Some(h) == guess.as_ref() {
                    format!("{h} (live ISO default)")
                } else {
                    h.clone()
                }
            })
            .collect();
        let idx = Select::new()
            .with_prompt("Which configuration to install?")
            .items(&labels)
            .default(preselect)
            .interact()
            .context("host selection aborted")?;
        let chosen = items[idx].clone();
        flake::valid_host_name(&chosen)?;
        return Ok(chosen);
    }
    if let Some(g) = guess {
        flake::valid_host_name(&g)?;
        return Ok(g);
    }
    if available.len() == 1 {
        return Ok(available[0].clone());
    }
    bail!(
        "cannot determine host: pass --host <name>{}",
        if available.is_empty() {
            String::new()
        } else {
            format!(" (available: {})", available.join(", "))
        }
    );
}

fn resolve_drive(args: &Args) -> Result<String> {
    if let Some(d) = &args.drive {
        return normalize_drive(d);
    }
    let disks = disk::list_disks()?;
    if disks.is_empty() {
        bail!("no disks found via lsblk");
    }
    let live = disk::live_source_disk()?;
    let labels: Vec<String> = disks
        .iter()
        .map(|d| {
            let flag = match live.as_deref() {
                Some(l) if *l == d.path => " [LIVE ISO — do not select]",
                _ => "",
            };
            let removable = if d.rm == "1" { " removable" } else { "" };
            let model = if d.model.is_empty() { "?" } else { &d.model };
            format!("{}  {}  {model}{removable}{flag}", d.path, d.size_h)
        })
        .collect();
    let idx = Select::new()
        .with_prompt("Select target drive (WILL BE WIPED)")
        .items(&labels)
        .default(0)
        .interact()
        .context("drive selection aborted")?;
    if live.as_deref() == Some(disks[idx].path.as_str()) {
        bail!(
            "refusing: {} backs the live ISO — pick another drive",
            disks[idx].path
        );
    }
    Ok(disks[idx].path.clone())
}

fn lookup_disk(drive: &str) -> Result<disk::Disk> {
    let disks = disk::list_disks()?;
    disks
        .into_iter()
        .find(|d| d.path == drive)
        .with_context(|| format!("drive {drive} not in `lsblk` disk list"))
}

fn normalize_drive(d: &str) -> Result<String> {
    let d = d.trim().to_string();
    if d.is_empty() {
        bail!("empty --drive value");
    }
    if d.starts_with("/dev/") {
        Ok(d)
    } else {
        Ok(format!("/dev/{d}"))
    }
}

fn confirm_wipe(args: &Args, drive: &str) -> Result<()> {
    println!("\n*** ALL DATA ON {drive} WILL BE DESTROYED ***");
    if args.dry_run {
        println!("(dry-run: confirmation skipped — nothing will be wiped)");
        return Ok(());
    }
    if args.yes {
        println!("(--yes: confirmation skipped)");
        return Ok(());
    }
    if !console_is_tty() {
        bail!("no TTY: pass --yes to confirm wiping {drive} non-interactively");
    }
    let ok = Confirm::new()
        .with_prompt(format!("Really wipe {drive} and install NixOS?"))
        .default(false)
        .interact()
        .context("confirmation aborted")?;
    if !ok {
        bail!("aborted by user — {drive} untouched");
    }
    // Second factor: re-type the drive path. Cheap insurance against mis-clicks.
    let typed: String = Input::new()
        .with_prompt(format!("Type the drive path ({drive}) to confirm"))
        .interact_text()
        .context("confirmation aborted")?;
    if typed.trim() != drive {
        bail!(
            "typed {:?} != {drive:?} — aborted, drive untouched",
            typed.trim()
        );
    }
    Ok(())
}

fn set_user_password() -> Result<()> {
    if !console_is_tty() {
        bail!(
            "no TTY for the password prompt; re-run with --no-set-password and set it later via `nixos-enter --root /mnt -c 'passwd loeclos'`"
        );
    }
    loop {
        let pw = Password::new()
            .with_prompt("Password for user loeclos")
            .with_confirmation("Confirm password", "passwords do not match — try again")
            .interact()
            .context("password prompt aborted")?;
        if pw.is_empty() {
            eprintln!("empty password not allowed — try again (or --no-set-password to skip)");
            continue;
        }
        if pw.contains('\n') {
            eprintln!("password must not contain a newline — try again");
            continue;
        }
        // chpasswd via nixos-enter; stdin piped (never on the cmdline).
        let creds = format!("loeclos:{pw}\n");
        cmd::run_with_stdin(
            Ctx::new(false),
            "nixos-enter",
            &["--root", "/mnt", "-c", "chpasswd"],
            &creds,
        )
        .context("failed to set loeclos password (system still installed; set it later with `nixos-enter --root /mnt -c 'passwd loeclos'`)")?;
        println!("password set for loeclos");
        return Ok(());
    }
}

fn console_is_tty() -> bool {
    // dialoguer/console check without adding a direct dep: /dev/tty opens => tty.
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extra(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn forbidden_flags_are_rejected() {
        for a in [
            "--flake",
            "--flake=/evil#x",
            "--fl",
            "--root",
            "--root=/evil",
            "--ro",
            "--system",
            "--closure",
            "--store-path",
            "--file",
            "-f",
            "--attr",
            "-A",
            "--channel",
            "--no-root-passwd",
            "--no-root-password",
            "--no-bootloader",
            "-vf",
            "-Af",
        ] {
            assert!(is_forbidden_install_arg(a), "{a} should be forbidden");
        }
    }

    #[test]
    fn build_options_stay_allowed() {
        for a in [
            "--impure",
            "--show-trace",
            "--sh",
            "--keep-going",
            "--log-format",
            "--option",
            "--max-jobs",
            "--no-channel-copy",
            "-j",
            "-v",
            "--verbose",
            "-I",
            "--offline",
        ] {
            assert!(!is_forbidden_install_arg(a), "{a} should be allowed");
        }
    }

    #[test]
    fn enforced_args_come_last_and_reject_overrides() {
        let staged = Path::new("/tmp/stage/dotfiles");
        let ok = build_install_args(staged, "desktop", &extra(&["--show-trace"]));
        let argv = ok.unwrap();
        assert_eq!(
            argv,
            vec![
                "--show-trace",
                "--flake",
                "/tmp/stage/dotfiles#desktop",
                "--root",
                "/mnt",
                "--no-root-passwd",
            ]
        );
        assert!(build_install_args(staged, "desktop", &extra(&["--root", "/evil"])).is_err());
        assert!(build_install_args(staged, "desktop", &extra(&["--flake=/e#x"])).is_err());
    }
}
