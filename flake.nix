{
  description = "levelup -- Wayland/Linux desktop tools (hugin, munin, mimir, sleipnir, valkyrie, heimdall, gorm, bragi)";

  inputs = {
    # Same channel as the NixOS flake that consumes this. When consumed from
    # there this input is overridden by inputs.nixpkgs.follows, so it only
    # governs standalone `nix build` / `nix develop` in this repo.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  };

  outputs =
    { nixpkgs, ... }:
    let
      # x86_64-linux only. Everything here is portable Linux and adding
      # "aarch64-linux" would be a one-word change, but nothing has ever built
      # it, so it is not promised. Darwin is out on the merits: Wayland, BlueZ,
      # PipeWire, /proc.
      systems = [ "x86_64-linux" ];

      # flake-utils would buy exactly this one line, at the price of an extra
      # input in every downstream lock. Same reasoning as the NixOS flake that
      # consumes this one, which keeps its input list to three.
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          levelup = pkgs.callPackage ./nix/package.nix { };
        in
        {
          inherit levelup;
          default = levelup;
        }
      );

      # The bridge into the NixOS flake. home-manager there runs with
      # useGlobalPkgs = true, so an overlay registered in any NixOS module
      # reaches home.packages too -- the only clean way to get a flake input
      # into a home module, since extraSpecialArgs passes only hostName.
      overlays.default = final: _prev: {
        levelup = final.callPackage ./nix/package.nix { };
      };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          # Deliberately the same native deps as nix/package.nix. A dev shell
          # that cannot build bragi or gorm is worse than none: it fails at the
          # end of a long compile instead of at entry.
          nativeBuildInputs = [
            pkgs.rustc
            pkgs.cargo
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
            pkgs.just # the justfile is the day-to-day entry point
            pkgs.pkg-config
            pkgs.rustPlatform.bindgenHook
          ];

          buildInputs = [
            pkgs.dbus
            pkgs.pipewire
          ];

          # `nix develop` prepends to PATH, so this toolchain shadows the one
          # home/albin.nix puts in the profile -- usually the same store paths
          # anyway, since both flakes track nixos-26.05.
          #
          # rust-analyzer resolves std sources through RUST_SRC_PATH; without
          # it "go to definition" into std silently does nothing.
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });

      # `nix fmt`. pkgs.nixfmt is the RFC-style formatter; nixfmt-rfc-style is
      # now just an alias that warns.
      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
