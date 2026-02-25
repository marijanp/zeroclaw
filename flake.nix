{
  inputs = {
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    nixpkgs.url = "nixpkgs/nixos-unstable";
  };

  outputs =
    {
      self,
      flake-utils,
      fenix,
      nixpkgs,
    }:
    let
      # there is also nixpkgs.lib.systems.flakeExposed
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      withPkgs =
        pkgsCallback:
        nixpkgs.lib.genAttrs supportedSystems (
          system:
          let
            pkgs = import nixpkgs {
              inherit system;
              overlays = [
                fenix.overlays.default
                (import ./overlay.nix)
              ];
            };
          in
          pkgsCallback { inherit pkgs system; }
        );
    in
    {
      formatter = withPkgs ({ pkgs, ... }: pkgs.nixfmt-tree);

      packages = withPkgs (
        { pkgs, system }:
        {
          default = self.packages.${system}.zeroclaw;
          inherit (pkgs) zeroclaw;
        }
      );

      devShells = withPkgs (
        { pkgs, ... }:
        {
          default = pkgs.mkShell {
            inputsFrom = [ pkgs.zeroclaw ];
            packages = [
              pkgs.rust-analyzer
            ];
          };
        }
      );
    };
}
