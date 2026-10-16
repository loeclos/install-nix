use clap::Parser;

/// Guided NixOS installer for the loeclos/dotfiles flake.
///
/// Interactively picks a target drive, partitions + formats it
/// (GPT: EFI + optional swap + ext4 root), mounts at /mnt, refreshes the
/// flake's hardware-configuration for the new UUIDs, then runs
/// `nixos-install --flake <flake>#<host>`.
#[derive(Debug, Parser)]
#[command(name = "install-nix", version, about)]
pub struct Args {
    /// Target drive (e.g. /dev/nvme0n1, /dev/sda). If omitted, you pick interactively.
    #[arg(long, value_name = "DRIVE")]
    pub drive: Option<String>,

    /// Flake host to install (e.g. desktop, laptop). If omitted, you pick
    /// interactively (defaults to the live ISO suffix: live-desktop -> desktop).
    #[arg(long, value_name = "HOST")]
    pub host: Option<String>,

    /// Flake source directory. Defaults to /iso/dotfiles on the live ISO,
    /// then ./dotfiles heuristics. Use this to test from a checkout.
    #[arg(long, value_name = "PATH")]
    pub flake: Option<String>,

    /// Swap partition size in GiB (0 disables swap, max 64).
    /// Validated with EFI size against disk capacity before anything is wiped.
    #[arg(long, default_value_t = 8, value_name = "GIB")]
    pub swap_size: u64,

    /// EFI partition size in MiB (128–4096).
    #[arg(long, default_value_t = 512, value_name = "MIB")]
    pub efi_size: u64,

    /// Skip the interactive confirmation (Confirm + re-type the drive path).
    /// DANGEROUS: the target disk is wiped without asking.
    #[arg(long, default_value_t = false)]
    pub yes: bool,

    /// Print the plan without changing anything. Read-only probes
    /// (lsblk, findmnt, id) still run; nothing is created, mounted,
    /// formatted, or wiped.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Skip setting the loeclos user password after install.
    /// By default install-nix prompts for a password and applies it via
    /// `nixos-enter --root /mnt -c chpasswd`.
    #[arg(long, default_value_t = false)]
    pub no_set_password: bool,

    /// Leave /mnt mounted after a successful install (default unmounts).
    #[arg(long, default_value_t = false)]
    pub no_unmount: bool,

    /// Reboot immediately after a successful install.
    #[arg(long, default_value_t = false)]
    pub reboot: bool,

    /// Extra args appended to `nixos-install` (e.g. -- --impure --show-trace).
    /// Everything after `--` is forwarded verbatim, except flags that would
    /// redirect the install (--flake, --root, --system, --file/--attr and
    /// their abbreviations, --no-bootloader, ...), which are rejected.
    /// install-nix always appends its own --flake/--root/--no-root-passwd last.
    #[arg(last = true, value_name = "NIXOS_INSTALL_ARGS")]
    pub install_args: Vec<String>,
}
