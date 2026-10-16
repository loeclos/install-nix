# install-nix — guided NixOS installer for loeclos/dotfiles
#
# Builds the `install-nix` CLI (Rust) and exposes it as a flake package +
# overlay so the dotfiles live ISO can ship it via:
#
#   inputs.install-nix.url = "github:loeclos/install-nix";
#   # in hosts/live.nix:
#   # environment.systemPackages = [ inputs.install-nix.packages.${system}.install-nix ];
#   # or via lib/overlays.nix.
{
  description = "install-nix — pick a drive, auto-partition, install the dotfiles NixOS flake";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAll = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAll (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          install-nix = pkgs.rustPlatform.buildRustPackage {
            pname = "install-nix";
            version = "0.1.0";
            src = ./.;

            cargoLock.lockFile = ./Cargo.lock;

            # No network during build: all crates come from cargoLock.
            # Runtime deps are shell-outs (parted, nixos-install, ...),
            # so no buildInputs needed.

            meta = {
              description = "Guided NixOS installer: select drive, partition/format, nixos-install a flake host";
              homepage = "https://github.com/loeclos/install-nix";
              license = pkgs.lib.licenses.mit;
              mainProgram = "install-nix";
              platforms = pkgs.lib.platforms.linux;
            };
          };
          default = self.packages.${system}.install-nix;
        }
      );

      # Consumed by loeclos/dotfiles lib/overlays.nix:
      #   install-nix.overlays.default
      overlays.default = final: prev: {
        install-nix = self.packages.${prev.system}.install-nix;
      };

      devShells = forAll (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              pkg-config
            ];
            RUST_BACKTRACE = "1";
          };
        }
      );

      formatter = forAll (system: nixpkgs.legacyPackages.${system}.nixfmt-rfc-style);
    };
}
