# install-nix

Guided NixOS installer for [loeclos/dotfiles](https://github.com/loeclos/dotfiles).
Boot the live ISO, run `install-nix`, pick a drive — it partitions, formats,
mounts, refreshes the flake's hardware config for the new UUIDs, and runs
`nixos-install` so the drive boots straight into your configuration.

## Usage (on the live ISO)

```bash
sudo install-nix
# interactive: pick host (desktop/laptop) + drive, confirm, re-type the drive path

# non-interactive example (password prompt needs a TTY — skip it with --no-set-password):
sudo install-nix --host desktop --drive /dev/nvme0n1 --flake /iso/dotfiles --yes --no-set-password

# dry run (prints the plan; read-only probes only, nothing created/mounted/wiped):
install-nix --dry-run

# forward extra flags to nixos-install:
sudo install-nix -- --show-trace
```

What it does:

1. Requires root, checks for its tools (`parted wipefs mkfs.fat mkfs.ext4
   mkswap swapon swapoff blkid lsblk findmnt mount mkdir umount cp chmod
   partprobe udevadm uname nixos-install nixos-generate-config nixos-enter`).
2. Resolves the flake (`--flake`, default `/iso/dotfiles` on the ISO).
3. Picks host (`--host`, default `live-<host>` → `<host>` from the ISO name)
   and validates it against the flake *before anything destructive*, so a
   typo can never cost a wiped disk.
4. Picks drive (`--drive`, else interactive `lsblk` list). The ISO's own
   device is detected via `findmnt /iso` and refused as a target; when
   `/iso` is mounted and its device cannot be resolved, install-nix aborts
   instead of guessing.
5. Validates EFI/swap sizes (EFI 128–4096 MiB, swap 0–64 GiB) and checks the
   layout fits the disk (plus an 8 GiB minimum root) before wiping.
6. Requires `/mnt` and `/mnt/boot` to be free, then confirms the wipe
   (unless `--yes`): Confirm + re-type the drive path.
7. Unmounts the target's own partitions/swap only, refuses to wipe while
   anything on it is still mounted, then GPT partitions (`1MiB→EFI` FAT32
   esp flag, optional swap, rest ext4 `root`), formats, and mounts
   `root→/mnt`, `esp→/mnt/boot` (`umask=077`), `swapon`.
8. Copies the flake to a writable temp dir (`.git` moved aside so the
   generated file is visible to flake evaluation), runs
   `nixos-generate-config --root /mnt` and overwrites
   `hosts/hardware/<host>.nix` (falls back to a blkid-synthesized file
   using the live machine's platform, never hardcoded x86_64).
9. Runs `nixos-install --flake <copy>#<host> --root /mnt --no-root-passwd`.
   Forwarded args cannot override the target: `--flake/--root/--system/
   --file/--attr` (incl. abbreviations) and `--no-bootloader` are rejected,
   and the enforced flags are appended last.
10. Prompts for the `loeclos` password, applies via
    `nixos-enter --root /mnt -c chpasswd` (skip with `--no-set-password`).
11. Unmounts exactly what it mounted (`/mnt/boot`, `/mnt`) plus its own
    swap partition only — never `umount -R` or `swapoff -a`. With `--reboot`,
    the reboot is issued only after a successful unmount.

Layout matches the installed hosts: GRUB EFI (`device = "nodev"`),
ext4 `/`, vfat `/boot`, swap partition — see
`hosts/hardware/{desktop,laptop}.nix` in dotfiles.

## Options

| Flag | Default | Meaning |
| --- | --- | --- |
| `--drive PATH` | interactive | Whole disk (`/dev/sda`, not `sda1`) |
| `--host NAME` | ISO guess | Flake host (`desktop`, `laptop`) |
| `--flake PATH` | `/iso/dotfiles` | Dotfiles checkout to install |
| `--swap-size GiB` | `8` | `0` = no swap partition (max 64) |
| `--efi-size MiB` | `512` | EFI partition size (128–4096) |
| `--yes` | off | Skip wipe confirmation (dangerous) |
| `--dry-run` | off | Print commands, change nothing |
| `--no-set-password` | off | Skip `loeclos` password setup |
| `--no-unmount` | off | Leave `/mnt` mounted |
| `--reboot` | off | Reboot after success |

## Wiring into dotfiles (live ISO)

In dotfiles, add the flake input and ship the binary on the ISO:

```nix
# flake.nix
inputs.install-nix.url = "github:loeclos/install-nix";

# hosts/live.nix (or lib/overlays.nix)
environment.systemPackages = [
  inputs.install-nix.packages.${system}.install-nix
];
```

Then rebuild the ISO:

```bash
nix build .#nixosConfigurations.live-desktop.config.system.build.isoImage
```

## Development

```bash
nix develop          # rust toolchain
cargo fmt && cargo clippy -- -D warnings
cargo test
cargo run -- --dry-run
nix flake check --no-build
nix build .#install-nix
```

## Safety

This tool wipes whole disks. It refuses partitions as targets, refuses the
live ISO's own device (and aborts if that device can't be determined), checks
the host, sizes, and `/mnt` before the wipe, requires typed confirmation, and
only ever unmounts mounts it created. `--yes` is only for scripted runs where
you already know the drive is disposable.
