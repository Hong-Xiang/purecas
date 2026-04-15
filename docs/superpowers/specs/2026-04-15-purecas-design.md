# purecas Design Spec

Content-addressable storage CLI for managing large binary datasets and model weights, inspired by Nix but designed for large files that don't belong in `/nix/store`.

## Architecture Overview

Single Rust crate producing a `pcas` binary, built and packaged via a nix flake. No new DSL or IR — dataset "recipes" are nix expressions in consuming repos that produce shell scripts calling `pcas`. Nix handles toolchain reproducibility; `pcas` handles content storage.

## Storage Layout

```
<CAS_ROOT>/
  sha256/<first 2 hex chars>/<full sha256 hex hash>   # blob files, mode 0o444
  purecas.db                                            # SQLite metadata
```

- `CAS_ROOT` defaults to `~/data/blob`, overridable via `$CAS_ROOT` env var or `--root` CLI flag.
- SHA-256 is the only hash algorithm. No dual-hash scheme.
- Blob files are stored read-only (0o444) to prevent accidental modification.
- `purecas.db` is created lazily on first write operation.
- Read-only commands (`path`, `cat`) work without a DB — they only need the filesystem.

## SQLite Schema

```sql
CREATE TABLE blobs (
    hash TEXT PRIMARY KEY
);

CREATE TABLE blob_names (
    hash TEXT NOT NULL REFERENCES blobs(hash),
    name TEXT NOT NULL,             -- basename only, never a full path
    UNIQUE(hash, name)
);

CREATE TABLE packages (
    name TEXT PRIMARY KEY,
    description TEXT
);

CREATE TABLE package_blobs (
    package_name TEXT NOT NULL REFERENCES packages(name),
    blob_hash TEXT NOT NULL REFERENCES blobs(hash),
    path TEXT,                      -- relative logical path within the package (organizational hint)
    UNIQUE(package_name, blob_hash)
);
```

- `blob_names.name`: original filename (basename only). One blob can accumulate multiple names from repeated `add` calls.
- `package_blobs.path`: relative path within the package for organizational purposes (e.g., `videos/001.mp4`). This is a human-friendly hint, not a filesystem path.

## CLI Commands

Binary name: `pcas`

### Global Options

```
--root <path>    Override CAS_ROOT (default: $CAS_ROOT or ~/data/blob)
```

### Blob Operations

```
pcas add <file>...
```
- Hash each file with SHA-256, copy to CAS using `cp --reflink=auto`, set permissions to 0o444.
- Record hash in `blobs` table, original filename (basename) in `blob_names`.
- Print `<hash> <filename>` per file to stdout.
- Idempotent: re-adding the same content is a no-op (new filename still recorded).

```
pcas fetch <url> --sha256 <expected_hash> [--unzip]
```
- Download URL to a temp file, compute SHA-256, verify against expected hash.
- On mismatch: error message, delete temp file, exit non-zero.
- On match: store in CAS, print hash.
- `--unzip`: extract archive contents, store each extracted file individually as its own blob, record each file's name from the archive in `blob_names`, print all hashes.

```
pcas path <hash>
```
- Print the expected filesystem path (`<CAS_ROOT>/sha256/<hash[:2]>/<hash>`).
- Indicate whether the file exists on disk (e.g., `<path> [exists]` or `<path> [missing]`).

```
pcas cat <hash>
```
- Write blob contents to stdout.
- Error if blob file does not exist.

### Package Operations

```
pcas pkg create <name> [--description <text>]
```
- Create a package entry in the DB.

```
pcas pkg add <name> <hash>... [--path <logical-path>]
```
- Associate blobs with a package.
- `--path` sets the logical path for the blob within the package. When `--path` is given with multiple hashes: error (path is per-blob, use separate calls).

```
pcas pkg list
```
- List all packages with blob count.

```
pcas pkg show <name>
```
- List blobs in the package: hash, logical path, known filenames from `blob_names`.

```
pcas pkg rm <name>
```
- Remove the package record and its `package_blobs` entries. Blob files are not deleted.

### Transfer Operations

```
pcas export <package> --to <dir>
pcas export <hash>... --to <dir>
```
- Copy blob files to `<dir>` preserving the `sha256/<prefix>/<hash>` layout.
- Write `<dir>/purecas-export.json` with package metadata and blob names:

```json
{
  "packages": [
    {
      "name": "sbd-rai",
      "description": "SBD RAI dataset",
      "blobs": [
        { "hash": "a3f2c1...", "path": "videos/001.mp4" },
        { "hash": "b7e4d9...", "path": "annotations/scene_001.txt" }
      ]
    }
  ],
  "blob_names": {
    "a3f2c1...": ["001.mp4", "rai_video_1.mp4"],
    "b7e4d9...": ["scene_001.txt"]
  }
}
```

- When exporting by hash (not package), `packages` array is empty, only `blob_names` is populated.

```
pcas import --from <dir>
```
- Read blob files from `<dir>` (following the `sha256/<prefix>/<hash>` layout), add to local CAS.
- If `purecas-export.json` is present, recreate package definitions and blob names.
- Merge semantics: existing packages gain new blobs, existing blob names are extended, nothing is overwritten.

## Nix Integration Pattern

The purecas repo's `flake.nix` outputs:
- `packages.default` — the `pcas` binary (built via `crane` or `naersk`)
- `devShells.default` — Rust toolchain + dev tools (clippy, rustfmt)

### Consumer-side usage

Consuming repos (e.g., VideoAnalysis-shot-research) add purecas as a flake input and write dataset recipes as nix expressions that produce shell scripts:

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
      apps.x86_64-linux.fetch-sbd = {
        type = "app";
        program = toString (pkgs.writeShellScript "fetch-sbd" ''
          set -euo pipefail
          ${pcas}/bin/pcas fetch "https://drive.google.com/..." \
            --sha256 a3f2c1deadbeef... --unzip
          ${pcas}/bin/pcas pkg create sbd-rai --description "SBD RAI dataset"
          ${pcas}/bin/pcas pkg add sbd-rai a3f2c1deadbeef... --path "videos/001.mp4"
        '');
      };
    };
}
```

Hashes are pre-calculated constants in the nix expression — same philosophy as nix fixed-output derivations, but data lands in CAS instead of `/nix/store`.

Usage: `nix run .#fetch-sbd`

## Rust Crate Structure

Single crate, single binary target.

### Dependencies

```toml
[dependencies]
clap = { version = "4", features = ["derive"] }
sha2 = "0.10"
rusqlite = { version = "0.31", features = ["bundled"] }
reqwest = { version = "0.12", features = ["blocking"] }
tempfile = "3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
zip = "2"
```

### Source Layout

```
src/
  main.rs          # clap CLI definition, subcommand dispatch
  store.rs         # blob storage: hash, copy, path resolution, read
  db.rs            # SQLite: open/init, blob_names, packages, package_blobs
  fetch.rs         # download + verify + optional unzip
  transfer.rs      # export/import logic
```

Each module maps to a command group. `store.rs` and `db.rs` are the shared core.

## Deferred (Future Work)

- `pcas gc` — remove blobs not referenced by any package
- Transport/sync protocols (SSH, rsync integration)
