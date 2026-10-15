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
        rustPlatform = pkgs.makeRustPlatform { cargo = rust; rustc = rust; };
      in {
        # The `subnet` binary for other projects (t4t, …): hub, node and CLI. Without the
        # default features: no embedded web UI and no tool router (fastembed downloads
        # ONNX runtime binaries at build time, which the Nix sandbox doesn't allow).
        packages.default = rustPlatform.buildRustPackage {
          pname = "subnet";
          version = "0.1.0";
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: _: !(builtins.elem (baseNameOf path) [ "target" "node_modules" "subnet-models" "vesper-data" ".direnv" ]);
          };
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "-p" "subnet" ];
          buildNoDefaultFeatures = true;
          SUBNET_WEBUI_BUILD = "0";
          doCheck = false; # the suite needs Postgres; run it with `cargo test` in the dev shell
          meta.mainProgram = "subnet";
        };
        devShells.default = pkgs.mkShell base;
        # personality-example at runtime: Piper for Vesper's voice.
        devShells.personality = pkgs.mkShell (base // { packages = base.packages ++ [ pkgs.piper-tts ]; });
        # Rebuilding personality-example/assets headless.
        devShells.assets = pkgs.mkShell { packages = [ pkgs.blender pkgs.python3 ]; };
      });
}
