{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      crane,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rustfmt"
            "clippy"
            "rust-src"
          ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain (_: rustToolchain);

        lofi = craneLib.buildPackage {
          src = craneLib.cleanCargoSource ./.;
          cargoToml = ./lofi/Cargo.toml;

          # rquickjs builds QuickJS via the `cc` crate.
          nativeBuildInputs = with pkgs; [
            gcc
          ];
        };
      in
      {
        packages.default = lofi;

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = [ rustToolchain ];

          packages = with pkgs; [
            # QuickJS (rquickjs) needs a C compiler at build time.
            gcc

            # Dev runner / watcher.
            cargo-watch
          ];
        };
      }
    );
}