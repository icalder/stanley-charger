{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];

        # Use the Nixpkgs example system for ARMv7
        pkgs = import nixpkgs {
          inherit system overlays;
          crossSystem = (import "${nixpkgs}/lib").systems.examples.armv7l-hf-multiplatform;
        };

        hostPkgs = import nixpkgs { inherit system overlays; };
        rustToolchain = hostPkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # This matches the triple Rust expects
        rustTarget = "armv7-unknown-linux-gnueabihf";
      in
      {
        devShells.default = hostPkgs.mkShell {
          nativeBuildInputs = [
            rustToolchain
            hostPkgs.pkg-config
            hostPkgs.patchelf
          ];

          buildInputs = [
            pkgs.stdenv.cc
            pkgs.dbus.dev
          ];

          shellHook = ''
            # Set the linker for Cargo
            export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER="${pkgs.stdenv.cc.targetPrefix}gcc"

            # Help pkg-config find the cross-compiled libraries (like dbus)
            export PKG_CONFIG_PATH="${pkgs.dbus.dev}/lib/pkgconfig"
            export PKG_CONFIG_ALLOW_CROSS=1

            echo "--- Cerbo GX (ARMv7) Cross-Shell Active ---"
            echo "Target: ${rustTarget}"
          '';
        };
      }
    );
}
