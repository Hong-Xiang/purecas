# purecas

Content-addressable storage for managing large binary datasets and model weights. Inspired by Nix, but designed for files that don't belong in `/nix/store`.

Files are stored by their SHA-256 hash. Identical content is never duplicated. Packages group related blobs (e.g., all files in a dataset) for easy management and transfer.

## Installation

### With Nix (recommended)

```bash
# Run directly
nix run github:Hong-Xiang/purecas#pcas -- --help

# Install into profile
nix profile install github:Hong-Xiang/purecas#pcas

# Or add to a flake as an input (see "Nix Integration" below)
```

### From source

```bash
git clone git@github.com:Hong-Xiang/purecas.git
cd purecas
nix develop   # enter dev shell with Rust toolchain
cargo build --release
# binary at target/release/pcas
```

## Configuration

The CAS root directory is resolved in this order:

1. `--root <path>` flag (per-command override)
2. `$CAS_ROOT` environment variable
3. `~/data/blob` (default)

### Storage layout

```
<CAS_ROOT>/
  sha256/<first 2 hex chars>/<full sha256 hash>   # blob files (read-only, 0o444)
  purecas.db                                        # SQLite metadata
```

## Usage

### Adding files

```bash
# Add one or more files to the store
pcas add model.pth dataset.zip

# Output: <sha256 hash> <filename> per file
# a3f2c1dead...  model.pth
# b7e4d9beef...  dataset.zip
```

Adding is idempotent -- re-adding the same file content is a no-op (but a new filename is still recorded).

### Fetching from a URL

```bash
# Download, verify SHA-256, and store
pcas fetch https://example.com/weights.pth --sha256 a3f2c1dead...

# Download a zip, verify, extract, and store each file individually
pcas fetch https://example.com/dataset.zip --sha256 b7e4d9beef... --unzip
```

The `--sha256` flag is required. If the downloaded file's hash doesn't match, the command fails and nothing is stored.

### Looking up files

```bash
# Print the filesystem path for a hash (and whether it exists)
pcas path a3f2c1dead...
# /home/user/data/blob/sha256/a3/a3f2c1dead...  [exists]

# Output blob contents to stdout
pcas cat a3f2c1dead... > restored_file.pth
```

### Packages

Packages group related blobs under a name. They are metadata-only -- removing a package does not delete the blob files.

```bash
# Create a package
pcas pkg create sbd-rai --description "SBD RAI shot boundary dataset"

# Add blobs to a package with optional logical paths
pcas pkg add sbd-rai a3f2c1dead... --path "videos/001.mp4"
pcas pkg add sbd-rai b7e4d9beef... --path "annotations/scene_001.txt"

# Add multiple blobs at once (no --path in this case)
pcas pkg add sbd-rai hash1 hash2 hash3

# List all packages
pcas pkg list
# sbd-rai    2 blobs

# Show blobs in a package
pcas pkg show sbd-rai
# a3f2c1dead...  videos/001.mp4 (001.mp4)
# b7e4d9beef...  annotations/scene_001.txt (scene_001.txt)

# Remove a package (blobs remain in the store)
pcas pkg rm sbd-rai
```

### Exporting and importing

Export copies blobs to a directory (preserving the `sha256/` layout) along with a `purecas-export.json` metadata file. Import reads from that directory with merge semantics.

```bash
# Export a package
pcas export sbd-rai --to /mnt/drive/sbd-export/

# Export specific hashes (no package context)
pcas export a3f2c1dead... b7e4d9beef... --to /tmp/blobs/

# Transfer using any tool you like
rsync -a /mnt/drive/sbd-export/ remote:/tmp/sbd-import/
# or: scp, cp, USB drive, etc.

# Import on the destination machine
pcas import --from /tmp/sbd-import/
# Imported 2 blob(s) from /tmp/sbd-import/
```

Import merges with existing data: packages gain new blobs, blob names are extended, nothing is overwritten.

### Using a custom root

```bash
# Per-command override
pcas --root /data/models add large_model.pth

# Or set the environment variable
export CAS_ROOT=/data/models
pcas add large_model.pth
```

## Nix Integration

purecas is designed to work with Nix flakes. The key idea: Nix handles reproducible toolchains and recipes, `pcas` handles content storage. Hashes are pre-calculated constants in nix expressions, just like fixed-output derivations -- but data lands in the CAS instead of `/nix/store`.

### Adding purecas to a project flake

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    purecas.url = "github:Hong-Xiang/purecas";
  };

  outputs = { self, nixpkgs, purecas, ... }:
    let
      pkgs = import nixpkgs { system = "x86_64-linux"; };
      pcas = purecas.packages.x86_64-linux.default;
    in {
      # Dataset fetch scripts as flake apps
      apps.x86_64-linux.fetch-sbd = {
        type = "app";
        program = toString (pkgs.writeShellScript "fetch-sbd" ''
          set -euo pipefail

          # Hashes are pre-calculated (like nix fixed-output derivations)
          ${pcas}/bin/pcas fetch "https://example.com/sbd-videos.zip" \
            --sha256 a3f2c1deadbeef... --unzip
          ${pcas}/bin/pcas fetch "https://example.com/sbd-annotations.zip" \
            --sha256 b7e4d9beefcafe... --unzip

          # Organize into a package
          ${pcas}/bin/pcas pkg create sbd-rai \
            --description "SBD RAI shot boundary dataset"
          ${pcas}/bin/pcas pkg add sbd-rai a3f2c1deadbeef... \
            --path "videos/001.mp4"
          ${pcas}/bin/pcas pkg add sbd-rai b7e4d9beefcafe... \
            --path "annotations/scene_001.txt"
        '');
      };

      # Include pcas in the dev shell
      devShells.x86_64-linux.default = pkgs.mkShell {
        buildInputs = [ pcas ];
      };
    };
}
```

### Using dataset recipes

```bash
# Download and organize a dataset (recipe is reproducible via nix)
nix run .#fetch-sbd

# Use the stored files
pcas pkg show sbd-rai
pcas path a3f2c1deadbeef...    # get filesystem path to use in scripts
```

## Development

```bash
nix develop            # enter dev shell
cargo build            # build
cargo test             # run all tests (19 unit + 12 integration)
cargo clippy           # lint
cargo fmt              # format
nix build .#pcas       # nix build
```

## Design Decisions

- **SHA-256 only** -- no multi-algorithm complexity, consistent with nix conventions.
- **Read-only blobs (0o444)** -- prevents accidental modification of stored content.
- **SQLite metadata** -- lightweight, embedded, no external dependencies. Tracks filenames and package membership.
- **Filesystem is the source of truth for content** -- the DB tracks metadata. `pcas path` and `pcas cat` work without a DB, only needing the blob files.
- **No built-in transport** -- export/import produces/consumes directories. Use rsync, scp, or any tool for transfer.
- **No garbage collection (yet)** -- planned for a future release.
