{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/release-25.05";
    systems.url = "github:nix-systems/default";

    utils = {
      url = "github:numtide/flake-utils";
      inputs.systems.follows = "systems";
    };

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";
  };

  outputs = { utils, ... }@inputs: utils.lib.eachDefaultSystem (system: let
    pkgs = inputs.nixpkgs.legacyPackages.${system};
    toolchain = inputs.fenix.packages.${system}.stable.toolchain;
    craneLib = (inputs.crane.mkLib pkgs).overrideToolchain toolchain;

    commonArgs = {
      src = craneLib.cleanCargoSource ./.;
      strictDeps = true;
    };

    relay = craneLib.buildPackage (commonArgs // {
      cargoArtifacts = craneLib.buildDepsOnly commonArgs;
    });
  in {
    checks = {
      inherit relay;
    };

    packages = rec {
      default = relay;
      inherit relay;
    };

    apps = rec {
      default = relayd;
      relayd = relay;
    };

    devShells.default = pkgs.mkShell {
      packages = [
        toolchain
      ];
    };
  });
}
