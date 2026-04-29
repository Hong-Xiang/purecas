# CLI Refactor & Library Crate Design

## Problem

The `purecas` CLI (`pcas`) has grown organically with subcommand naming that doesn't clearly convey intent (`add` vs `fetch`), redundant commands (`cat`, existence checks in `path`), and no programmatic API — all logic lives in a binary crate with private modules. A Python project needs to access CAS operations natively via PyO3.

## Approach

Restructure into a Cargo workspace with three members:

1. **`purecas`** — library crate exposing `Store`, `Blob`, `Package` types with all core CAS operations
2. **`pcas`** — thin CLI binary that depends on `purecas`, handles arg parsing and output
3. **`purecas-python`** — PyO3 crate wrapping `purecas` for Python, built with maturin

Rename CLI subcommands to be explicit about their data source, remove redundant commands, and follow Unix philosophy (let the shell handle existence checks and file reading).

## Workspace Layout

```
purecas/                    (workspace root)
├── Cargo.toml              (workspace manifest)
├── purecas/                (library crate)
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs          (pub mod + Store type)
│       ├── store.rs        (blob storage: hash, store, path resolution)
│       ├── db.rs           (SQLite metadata)
│       ├── fetch.rs        (HTTP download + verify)
│       ├── transfer.rs     (export/import)
│       └── lfs.rs          (Git LFS transfer agent protocol)
├── pcas/                   (CLI binary)
│   ├── Cargo.toml          (depends on purecas)
│   └── src/main.rs         (clap + thin dispatch)
└── purecas-python/         (PyO3 bindings)
    ├── Cargo.toml
    ├── pyproject.toml       (maturin config)
    └── src/lib.rs
```

## Rust Library API (`purecas` crate)

### Core Types

```rust
pub struct Store {
    root: PathBuf,
    conn: rusqlite::Connection,
}

pub struct Blob<'a> {
    store: &'a Store,
    hash: String,
}

pub struct Package<'a> {
    store: &'a Store,
    name: String,
}
```

### `Store` Methods

```rust
impl Store {
    /// Open (or create) a CAS store at the given root directory.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self>;

    /// Get a Blob handle by hash. Cheap — no db/fs check.
    pub fn blob(&self, hash: &str) -> Blob<'_>;

    // --- Add from local paths ---
    pub fn add_path(&self, path: &Path) -> Result<Blob<'_>>;
    pub fn add_verified_path(&self, path: &Path, expected_hash: &str) -> Result<Blob<'_>>;

    // --- Add from URLs ---
    pub fn add_url(&self, url: &str) -> Result<Blob<'_>>;
    pub fn add_verified_url(&self, url: &str, expected_hash: &str) -> Result<Blob<'_>>;
    pub fn add_url_unzip(&self, url: &str) -> Result<Vec<Blob<'_>>>;
    pub fn add_verified_url_unzip(&self, url: &str, expected_hash: &str) -> Result<Vec<Blob<'_>>>;

    // --- Package operations ---
    pub fn create_package(&self, name: &str, desc: Option<&str>) -> Result<Package<'_>>;
    pub fn package(&self, name: &str) -> Package<'_>;
    pub fn list_packages(&self) -> Result<Vec<Package<'_>>>;

    // --- Transfer ---
    pub fn import(&self, from: &Path) -> Result<ImportResult>;
}
```

### `Blob` Methods

```rust
impl<'a> Blob<'a> {
    /// The SHA-256 hash of this blob.
    pub fn hash(&self) -> &str;

    /// Filesystem path where this blob is stored. Just the path — no existence check.
    pub fn path(&self) -> PathBuf;

    /// Known file names for this blob.
    pub fn names(&self) -> Result<Vec<String>>;

    // --- Tags ---
    pub fn add_tags(&self, tags: &[&str]) -> Result<()>;
    pub fn tags(&self) -> Result<Vec<String>>;

    // --- Metadata ---
    pub fn set_metadata(&self, value: &str) -> Result<()>;
    pub fn metadata(&self) -> Result<Option<String>>;

    // --- Relations ---
    pub fn add_relation(&self, target: &Blob, note: Option<&str>) -> Result<()>;
    pub fn relations(&self) -> Result<Vec<Relation>>;
}
```

### `Package` Methods

```rust
impl<'a> Package<'a> {
    pub fn name(&self) -> &str;
    pub fn description(&self) -> Result<Option<String>>;
    pub fn add_blob(&self, blob: &Blob, path: Option<&str>) -> Result<()>;
    pub fn blobs(&self) -> Result<Vec<PackageBlobInfo>>;
    pub fn remove(&self) -> Result<()>;
    pub fn export(&self, to: &Path) -> Result<()>;
}
```

### Design Decisions

- **`blob(hash)` is cheap**: Constructs a `Blob` handle without hitting db or filesystem. Consistent with the Unix philosophy — `path` just returns a path.
- **Verified variants**: `add_verified_*` methods compute the hash after storing and return an error if it doesn't match the expected hash.
- **Unzip as separate methods**: Rather than a bool flag, `add_url_unzip` and `add_verified_url_unzip` are distinct methods. Clearer intent, easier to discover.
- **No `export_blobs`**: Exporting individual blobs is just `cp $(pcas path HASH) dest/` — let the shell handle it. Only package export (which bundles metadata) warrants a dedicated method.

## CLI Subcommand Mapping

| Old Command | New Command | Behavior Change |
|---|---|---|
| `add <files>` | `add-path <files>` | Renamed for clarity |
| `fetch <url> --sha256 <hash>` | `add-url <url>` | `--sha256` optional (verified if provided), `--unzip` flag selects unzip variant |
| `path <hash>` | `path <hash>` | Prints only the path — no `[exists]`/`[missing]` suffix |
| `cat <hash>` | *(removed)* | Use `cat $(pcas path HASH)` |
| `tag` | `tag` | Unchanged |
| `meta` | `meta` | Unchanged |
| `rel` | `rel` | Unchanged |
| `pkg` | `pkg` | Unchanged |
| `export <targets> --to <dir>` | `export <package> --to <dir>` | Package only — no blob hash targets |
| `import --from <dir>` | `import --from <dir>` | Unchanged |
| `lfs-agent` | `lfs-agent` | Unchanged |

**No backward compatibility**: Clean break. Old command names are removed entirely, not aliased.

## PyO3 Python Bindings (`purecas-python`)

### Ownership Model

Rust `Blob<'a>` and `Package<'a>` borrow `&Store`, but PyO3 cannot express Rust lifetimes. The Python wrappers use `Arc<purecas::Store>` for shared ownership:

```rust
#[pyclass]
struct Store {
    inner: Arc<purecas::Store>,
}

#[pyclass]
struct Blob {
    store: Arc<purecas::Store>,
    hash: String,
}

#[pyclass]
struct Package {
    store: Arc<purecas::Store>,
    name: String,
}
```

The Rust `Store` uses `&self` for all operations. For PyO3 thread safety, the `rusqlite::Connection` inside `Store` will be wrapped in a `Mutex`.

### Python API

```python
import purecas

store = purecas.Store.open("/path/to/cas")

# Add blobs
blob = store.add_path("/data/file.bin")
blob = store.add_verified_path("/data/file.bin", "abc123...")
blob = store.add_url("https://example.com/file.bin")
blob = store.add_verified_url("https://example.com/file.bin", "abc123...")
blobs = store.add_url_unzip("https://example.com/archive.zip")

# Blob properties and methods
blob.hash          # str
blob.path          # str
blob.names()       # list[str]
blob.add_tags(["train", "v2"])
blob.tags()        # list[str]
blob.set_metadata("epoch=10")
blob.metadata()    # str | None
blob.add_relation(other_blob, note="derived-from")

# Retrieve existing blob
blob = store.blob("abc123...")

# Packages
pkg = store.create_package("my-dataset", description="training images")
pkg.add_blob(blob, path="images/001.png")
pkg.blobs()        # list[dict]
pkg.export("/tmp/export")
pkg.remove()

# List packages
for pkg in store.list_packages():
    print(pkg.name)

# Import
store.import_from("/tmp/export")  # `import` is a Python keyword
```

### Build & Distribution

- Built with **maturin** (`maturin develop` for local, `maturin build` for wheels)
- Published to PyPI as `purecas`
- `pyproject.toml` configures maturin with the `pyo3` binding type

## Error Handling

- The Rust library uses `anyhow::Result` for all fallible operations (consistent with current codebase)
- PyO3 bindings convert `anyhow::Error` to Python `RuntimeError` via `pyo3::exceptions`
- The CLI binary prints errors to stderr and exits with code 1

## Testing

- **Library crate**: Existing unit tests in `store.rs` and integration tests move here. Add tests for the new `Store`/`Blob`/`Package` API surface.
- **CLI binary**: Integration tests that invoke `pcas` and verify output. Existing CLI integration tests in `tests/` are adapted for new subcommand names.
- **PyO3**: Basic smoke tests using `maturin develop` + pytest to verify the Python API works end-to-end.
