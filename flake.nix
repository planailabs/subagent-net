{
  description = "subagent-net: distributed network of resumable LLM agents";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, flake-utils, rust-overlay, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; overlays = [ rust-overlay.overlays.default ]; };
        rust = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
        };
        base = {
          # cmake and libclang build whisper.cpp for personality-example's stt stage.
          packages = [ rust pkgs.postgresql pkgs.sqlx-cli pkgs.nodejs_22 pkgs.cmake ];
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
        };
      in {
        devShells.default = pkgs.mkShell base;
        # personality-example at runtime: Piper for Vesper's voice.
        devShells.personality = pkgs.mkShell (base // { packages = base.packages ++ [ pkgs.piper-tts ]; });
        # Rebuilding personality-example/assets headless.
        devShells.assets = pkgs.mkShell { packages = [ pkgs.blender pkgs.python3 ]; };
      });
}
