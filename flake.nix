{
  description = "purecas — content-addressable storage for datasets and model weights";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, crane, rust-overlay, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      rustToolchain = pkgs.rust-bin.stable.latest.default;
      craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

      src = craneLib.cleanCargoSource ./.;

      commonArgs = {
        inherit src;
        pname = "pcas";
        version = "0.1.0";
        strictDeps = true;
        nativeBuildInputs = with pkgs; [ pkg-config ];
        buildInputs = with pkgs; [ openssl ];
        # Only build purecas + pcas; purecas-python needs Python and is built via maturin
        cargoExtraArgs = "--workspace --exclude purecas-python";
      };

      cargoArtifacts = craneLib.buildDepsOnly commonArgs;

      pcas = craneLib.buildPackage (commonArgs // {
        inherit cargoArtifacts;
        meta.mainProgram = "pcas";
      });
    in
    {
      packages.${system} = {
        default = pcas;
        pcas = pcas;
      };

      devShells.${system}.default = craneLib.devShell {
        packages = with pkgs; [
          rust-analyzer
          pkg-config
          openssl
          maturin
        ];
      };
    };
}
