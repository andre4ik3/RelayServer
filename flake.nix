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
  };

  outputs = { utils, ... }@inputs: utils.lib.eachDefaultSystem (system: let
    pkgs = inputs.nixpkgs.legacyPackages.${system};
    fenix = inputs.fenix.packages.${system};
  in {
    devShells.default = pkgs.mkShell {
      packages = [
        (fenix.stable.toolchain)
        pkgs.nil
      ];
    };
  });
}
