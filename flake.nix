{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane/edb38893982a3338972bb4a2ec7ce7c29ba10fd9";
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

        # crane's cleanCargoSource drops non-Rust assets, but lofi-core
        # embeds prompts/system.md via include_str!, so keep .md files too.
        src = pkgs.lib.cleanSourceWith {
          src = craneLib.path ./.;
          filter = path: type:
            (craneLib.filterCargoSources path type)
            || (pkgs.lib.hasSuffix ".md" path);
        };

        lofi = craneLib.buildPackage {
          inherit src;
          cargoToml = ./lofi/Cargo.toml;
          version = "0.1.0";

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

            # Binary size analysis.
            cargo-bloat
          ];
        };
      }
    );
}