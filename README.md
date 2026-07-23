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
pcas add-path model.pth dataset.zip

# Output: <sha256 hash> <filename> per file
# a3f2c1dead...  model.pth
# b7e4d9beef...  dataset.zip
```

Adding is idempotent -- re-adding the same file content is a no-op (but a new filename is still recorded).

### Fetching from a URL

```bash
# Download, verify SHA-256, and store
pcas add-url https://example.com/weights.pth --sha256 a3f2c1dead...

# Download without hash verification (hash computed after download)
pcas add-url https://example.com/weights.pth

# Download a zip, verify, extract, and store each file individually
pcas add-url https://example.com/dataset.zip --sha256 b7e4d9beef... --unzip
```

If `--sha256` is provided and the downloaded file's hash doesn't match, the command fails and nothing is stored.

### Filesystem object index

purecas is transitioning to a filesystem-first design (see the design
issue for the full plan). `pcas index` is the single reconciliation
command: it discovers regular files under `PCAS_ROOT`, hashes and
deduplicates their content onto one hard-linked object entry per distinct
SHA-256 digest under `.pcas/sha256/<first2>/`, repairs object entries
whose bytes changed, and prunes entries with no remaining hard link.
It never touches `purecas.db`.

```bash
# Index every visible file under the CAS root
pcas index

# Index only files matching a pattern (basename, or root-relative if it
# contains a '/')
pcas index '*.mp4'
pcas index 'datasets/train/*.bin'

# Force full content verification instead of trusting an indexed file's
# mtime (see the caveat below)
pcas index --rehash
```

The command takes an exclusive, non-blocking lock on `.pcas/index.lock`
for its entire run; a concurrent `pcas index` fails immediately instead
of racing. It prints one line per newly created object entry, then a
summary:

```text
indexed=1 reused=2 deduplicated=1 repaired=0 pruned=0 failed=0
```

Any independent per-path failure (an unreadable or unstable file, a
failed link/rename/verify, or a failed prune) is printed to stderr;
`pcas index` still processes every other path, but exits non-zero
whenever `failed > 0`.

**Non-adversarial mtime caveat:** to avoid rehashing unchanged content on
every run, an already-indexed file is trusted without hashing when its
mtime is not later than the time it was indexed. This is intentionally
not adversarial: a tool that preserves or backdates mtime across a
content change (`cp -p`, `rsync -a`, some archive extractors) can defeat
it silently. Run `pcas index --rehash` after using such a tool, or
whenever you need a guaranteed full verification.

`pcas path <hash>` (below) now resolves exclusively against these
`.pcas` object entries: it requires the content to have been indexed with
`pcas index` first, fails if the digest is unknown, and never opens
`purecas.db`. It is no longer related to `pcas add-path`'s `sha256/`
layout.

### Looking up files

```bash
# Print the filesystem path for a hash indexed with `pcas index`
pcas path a3f2c1dead...
# /home/user/data/blob/.pcas/sha256/a3/a3f2c1dead...--20260722T130016.139Z

# Read blob contents via shell pipe
cat $(pcas path a3f2c1dead...) > restored_file.pth
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

# Copy individual blobs with shell
cp $(pcas path a3f2c1dead...) /tmp/blobs/

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
pcas --root /data/models add-path large_model.pth

# Or set the environment variable
export CAS_ROOT=/data/models
pcas add-path large_model.pth
```

### Serving the visible hierarchy over HTTP

`pcas serve` exposes every visible file and directory under `PCAS_ROOT` as
a read-only HTTP server, binding to `127.0.0.1:8000` by default:

```bash
pcas serve
# pcas serve: listening on http://127.0.0.1:8000

pcas --root /data/models serve --bind 0.0.0.0:9000
```

```bash
curl http://127.0.0.1:8000/datasets/train/001.bin
curl http://127.0.0.1:8000/datasets/train/   # directory listing
```

`serve` is dispatched before `purecas.db` is ever opened, so it never
creates or reads the legacy SQLite database, and it never exposes it if it
already exists at the root. `.pcas` (the internal object store) and the
top-level `/pcas` path (reserved for the future digest route) are never
served; nested directories literally named `pcas` remain visible. `GET`
and `HEAD` support full representation metadata (`Content-Length`,
`Content-Type`, `Last-Modified`, `Accept-Ranges`, a weak `ETag`) and RFC
9110 conditional requests (`If-Match`, `If-Unmodified-Since`,
`If-None-Match`, `If-Modified-Since`). Byte-range requests, the
`/pcas/<hash>` digest route, and a strong immutable `ETag` are deferred to
a follow-up slice.

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
          ${pcas}/bin/pcas add-url "https://example.com/sbd-videos.zip" \
            --sha256 a3f2c1deadbeef... --unzip
          ${pcas}/bin/pcas add-url "https://example.com/sbd-annotations.zip" \
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
cargo test             # run all tests (54 unit + 19 integration)
cargo clippy           # lint
cargo fmt              # format
nix build .#pcas       # nix build
```

## Library Usage (Rust)

purecas is also a library crate. Add to your `Cargo.toml`:

```toml
[dependencies]
purecas = { path = "purecas" }
```

```rust
use purecas::Store;

let store = Store::open("/path/to/cas")?;

// Add a file
let blob = store.add_path("data/file.bin")?;
println!("Stored: {}", blob.hash());

// Metadata
blob.add_tags(&["train", "v2"])?;
blob.set_metadata("epoch=10")?;

// Packages
let pkg = store.create_package("my-dataset", Some("training data"))?;
pkg.add_blob(&blob, Some("images/001.png"))?;
pkg.export("/tmp/export")?;
```

## Python Usage

Install with maturin:

```bash
cd purecas-python
maturin develop
```

```python
import purecas

store = purecas.Store.open("/path/to/cas")
blob = store.add_path("/data/file.bin")
print(blob.hash, blob.path)

blob.add_tags(["train", "v2"])
blob.set_metadata("epoch=10")

pkg = store.create_package("my-dataset")
pkg.add_blob(blob, path="images/001.png")
pkg.export("/tmp/export")
```

## Design Decisions

- **SHA-256 only** -- no multi-algorithm complexity, consistent with nix conventions.
- **Read-only blobs (0o444)** -- prevents accidental modification of stored content.
- **SQLite metadata** -- lightweight, embedded, no external dependencies. Tracks filenames and package membership.
- **Filesystem is the source of truth for content** -- the DB tracks metadata. `pcas path` and `pcas cat` work without a DB, only needing the blob files.
- **No built-in transport** -- export/import produces/consumes directories. Use rsync, scp, or any tool for transfer. Git LFS integration is available for version-controlled workflows.
- **`pcas serve` on axum/Tokio** -- the visible-hierarchy HTTP server (see above) uses current stable `axum`/Tokio, already present transitively through `reqwest`; every file is opened exactly once and representation metadata/body bytes both come from that same descriptor, so `tower-http`'s path-only `ServeFile`/`ServeDir` (which would reopen a path after deriving metadata) are deliberately not used. The async runtime is entered only for this command.
- **No garbage collection (yet)** -- planned for a future release.

## Git LFS Integration

`pcas` can act as a [Git LFS custom transfer agent](https://github.com/git-lfs/git-lfs/blob/main/docs/custom-transfers.md), allowing Git LFS to store large files in your purecas CAS instead of a remote server.

### Setup

Add to your repo's `.git/config` (or global `~/.gitconfig`):

```gitconfig
[lfs "customtransfer.pcas"]
    path = pcas
    args = "lfs-agent"
[lfs]
    standalonetransferagent = pcas
```

If your CAS root isn't the default (`~/data/blob`), pass it via args:

```gitconfig
[lfs "customtransfer.pcas"]
    path = pcas
    args = "--root /path/to/cas lfs-agent"
```

Or set the `CAS_ROOT` environment variable.

### How it works

When you `git push` or `git pull`, Git LFS spawns `pcas lfs-agent` and communicates via a JSON protocol over stdin/stdout:

- **Upload (`git push`):** Stores the blob in the CAS (`sha256/<prefix>/<hash>`), verifies the hash matches the LFS OID, and registers it in the metadata DB. Progress is reported during the transfer.
- **Download (`git pull`):** Returns the CAS path for the blob, which Git LFS reads directly.

### Manual testing

```bash
echo '{"event":"init","operation":"upload","remote":"origin","concurrent":false}' \
  | pcas lfs-agent
# → {"event":"init"}
```
