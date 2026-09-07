{
  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs =
    {
      self,
      flake-utils,
      nixpkgs,
    }:
    {
      nixosModules.nexigon-agent = ./nix/nexigon-agent.nix;
      nixosModules.default = self.nixosModules.nexigon-agent;
      overlays.default = final: _prev: {
        nexigon-agent = self.packages.${final.stdenv.hostPlatform.system}.nexigon-agent;
      };
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = (import nixpkgs) {
          inherit system;
        };
      in
      {
        packages.nexigon-agent = self.packages.${system}.default.overrideAttrs {
          cargoBuildFlags = [
            "--bin"
            "nexigon-agent"
          ];
          # Workspace filesystem-ownership tests require a conventional root filesystem.
          doCheck = false;
          meta.mainProgram = "nexigon-agent";
        };

        checks = pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          nixos = import ./nix/test.nix {
            inherit pkgs;
            module = self.nixosModules.nexigon-agent;
            agent = self.packages.${system}.nexigon-agent;
          };
        };

        packages.default = pkgs.rustPlatform.buildRustPackage {
          name = "nexigon";
          src = ./.;

          env.NEXIGON_GIT_VERSION = self.shortRev or self.dirtyShortRev or "unknown";

          cargoLock = {
            lockFile = ./Cargo.lock;
            allowBuiltinFetchGit = true;
          };
        };
      }
    );
}
