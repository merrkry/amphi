{
  description = "Nix build coordinator for GitHub repositories";

  nixConfig = {
    extra-substituters = [ "https://niks3.tsubasa.moe" ];
    extra-trusted-public-keys = [
      "niks3.tsubasa.moe-1:tPUzYtlqW1bgA/9otIZA9LLmwESVaEmHCrEl6pbCxOY="
    ];
  };

  inputs.nixpkgs.url = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.zst";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems =
        f: lib.genAttrs systems (system: f (nixpkgs.legacyPackages.${system}.extend self.overlays.default));
    in
    {
      overlays.default = final: _: {
        amphi = final.callPackage ./nix/package.nix { };
        amphi-web = final.callPackage ./nix/web.nix { };
      };

      nixosModules.default = ./nix/module.nix;

      packages = forAllSystems (pkgs: {
        inherit (pkgs) amphi amphi-web;
        default = pkgs.amphi;
      });

      checks = forAllSystems (
        pkgs:
        {
          inherit (pkgs) amphi amphi-web;
        }
        # The VM test builds x86_64-linux derivations inside its VMs.
        // lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
          vm = pkgs.amphi.tests.vm;
        }
      );

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rs);
    };
}
