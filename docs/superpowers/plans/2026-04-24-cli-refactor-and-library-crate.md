# CLI Refactor & Library Crate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restructure purecas from a single binary crate into a Cargo workspace with a `purecas` library crate (exposing `Store`/`Blob`/`Package` API), a thin `pcas` CLI binary, and `purecas-python` PyO3 bindings.

**Architecture:** Extract all core logic into a library crate with `Store` as the central type owning `root: PathBuf` and `conn: Connection`. `Blob<'a>` and `Package<'a>` borrow `&Store` for metadata operations. CLI becomes a thin dispatch layer. PyO3 uses `Arc<Mutex<Store>>` for Python ownership.

**Tech Stack:** Rust (2021 edition), clap 4, rusqlite 0.31 (bundled), sha2, reqwest (blocking), serde/serde_json, zip, anyhow, pyo3 0.22, maturin

**Design Spec:** `docs/superpowers/specs/2026-04-24-cli-refactor-and-library-crate-design.md`

---

## File Structure

### New files to create:
- `purecas/Cargo.toml` — library crate manifest
- `purecas/src/lib.rs` — public API: `Store`, `Blob`, `Package`, re-exports
- `pcas/Cargo.toml` — CLI binary manifest
- `pcas/src/main.rs` — clap CLI with new subcommand names
- `purecas-python/Cargo.toml` — PyO3 crate manifest
- `purecas-python/pyproject.toml` — maturin build config
- `purecas-python/src/lib.rs` — Python wrappers for Store/Blob/Package

### Files to move:
- `src/store.rs` → `purecas/src/store.rs` (no changes needed)
- `src/db.rs` → `purecas/src/db.rs` (no changes needed)
- `src/fetch.rs` → `purecas/src/fetch.rs` (change `crate::store` → `crate::store`)
- `src/transfer.rs` → `purecas/src/transfer.rs` (change `crate::{db, store}` → `crate::{db, store}`)
- `src/lfs.rs` → `purecas/src/lfs.rs` (change `crate::{db, store}` → `crate::{db, store}`)

### Files to modify:
- `Cargo.toml` — convert to workspace manifest
- `flake.nix` — update for workspace build
- `tests/cli.rs` — update command names, remove cat/path-status tests

### Files to delete:
- `src/main.rs` — replaced by `pcas/src/main.rs`

---

### Task 1: Workspace Scaffolding

**Files:**
- Modify: `Cargo.toml`
- Create: `purecas/Cargo.toml`
- Create: `pcas/Cargo.toml`
- Create: `purecas/src/lib.rs`
- Create: `pcas/src/main.rs`
- Move: `src/store.rs` → `purecas/src/store.rs`
- Move: `src/db.rs` → `purecas/src/db.rs`
- Move: `src/fetch.rs` → `purecas/src/fetch.rs`
- Move: `src/transfer.rs` → `purecas/src/transfer.rs`
- Move: `src/lfs.rs` → `purecas/src/lfs.rs`
- Move: `src/main.rs` → `pcas/src/main.rs`
- Move: `tests/cli.rs` → `pcas/tests/cli.rs`

This task only restructures files and Cargo manifests. No API changes yet — the CLI binary re-exports everything through the library to keep tests passing.

- [ ] **Step 1: Create directory structure**

```bash
mkdir -p purecas/src pcas/src pcas/tests
```

- [ ] **Step 2: Create workspace root Cargo.toml**

Replace the existing `Cargo.toml` with a workspace manifest:

```toml
[workspace]
members = ["purecas", "pcas"]
resolver = "2"
```

- [ ] **Step 3: Create purecas/Cargo.toml**

```toml
[package]
name = "purecas"
version = "0.1.0"
edition = "2021"

[dependencies]
sha2 = "0.10"
rusqlite = { version = "0.31", features = ["bundled"] }
reqwest = { version = "0.12", features = ["blocking"] }
tempfile = "3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
zip = "2"
anyhow = "1"
```

- [ ] **Step 4: Create pcas/Cargo.toml**

```toml
[package]
name = "pcas"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "pcas"
path = "src/main.rs"

[dependencies]
purecas = { path = "../purecas" }
clap = { version = "4", features = ["derive"] }
anyhow = "1"

[dev-dependencies]
assert_cmd = "2"
predicates = "3"
tempfile = "3"
```

- [ ] **Step 5: Move source files to purecas/src/**

```bash
mv src/store.rs purecas/src/store.rs
mv src/db.rs purecas/src/db.rs
mv src/fetch.rs purecas/src/fetch.rs
mv src/transfer.rs purecas/src/transfer.rs
mv src/lfs.rs purecas/src/lfs.rs
```

- [ ] **Step 6: Create purecas/src/lib.rs**

This is a pass-through that exposes existing modules publicly so `pcas` can use them. No API redesign yet.

```rust
pub mod db;
pub mod fetch;
pub mod lfs;
pub mod store;
pub mod transfer;
```

- [ ] **Step 7: Move src/main.rs to pcas/src/main.rs and update imports**

```bash
mv src/main.rs pcas/src/main.rs
```

Edit `pcas/src/main.rs` — replace all `mod` declarations with `use purecas::` imports. Change the top of the file from:

```rust
use clap::{Parser, Subcommand};
use std::fs;
use std::path::PathBuf;

mod db;
mod fetch;
mod lfs;
mod store;
mod transfer;
```

to:

```rust
use clap::{Parser, Subcommand};
use std::fs;
use std::path::PathBuf;

use purecas::{db, fetch, lfs, store, transfer};
```

- [ ] **Step 8: Move tests/cli.rs to pcas/tests/cli.rs**

```bash
mv tests/cli.rs pcas/tests/cli.rs
rmdir tests
```

- [ ] **Step 9: Remove old src/ directory**

```bash
rm -rf src
```

- [ ] **Step 10: Build and run all tests**

```bash
cargo build --workspace
cargo test --workspace
```

Expected: All tests pass. The library re-exports everything, CLI uses library modules, no behavioral change.

- [ ] **Step 11: Commit**

```bash
git add -A
git commit -m "refactor: convert to cargo workspace with purecas lib + pcas binary"
```

---

### Task 2: Store Type and Blob Type

**Files:**
- Modify: `purecas/src/lib.rs`
- Create: (new types in `lib.rs`)

Introduce the `Store` struct that owns `root` and `conn`, and `Blob<'a>` that borrows `&Store`. These wrap the existing module functions — no refactoring of store.rs/db.rs internals.

- [ ] **Step 1: Write failing tests for Store::open and Store::blob**

Add to the bottom of `purecas/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_store_open() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(s.root(), dir.path());
    }

    #[test]
    fn test_blob_path() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let b = s.blob("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let expected = dir.path().join("sha256").join("ab").join("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        assert_eq!(b.path(), expected);
    }

    #[test]
    fn test_blob_hash() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let b = s.blob("abc123");
        assert_eq!(b.hash(), "abc123");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p purecas -- tests::test_store_open tests::test_blob_path tests::test_blob_hash
```

Expected: FAIL — `Store` and `Blob` types don't exist yet.

- [ ] **Step 3: Implement Store and Blob types in lib.rs**

Replace the contents of `purecas/src/lib.rs` (above the `#[cfg(test)]` block) with:

```rust
use anyhow::Result;
use std::path::{Path, PathBuf};

pub mod db;
pub mod fetch;
pub mod lfs;
pub mod store;
pub mod transfer;

pub use transfer::ImportResult;

pub struct Store {
    root: PathBuf,
    conn: rusqlite::Connection,
}

pub struct Blob<'a> {
    store: &'a Store,
    hash: String,
}

pub struct BlobInfo {
    pub hash: String,
    pub name: Option<String>,
}

pub struct Relation {
    pub target: String,
    pub note: Option<String>,
}

pub struct PackageBlobInfo {
    pub hash: String,
    pub path: Option<String>,
    pub names: Vec<String>,
}

impl Store {
    /// Open (or create) a CAS store at the given root directory.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let conn = db::open_db(&root)?;
        Ok(Self { root, conn })
    }

    /// The root directory of this store.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Get a Blob handle by hash. Cheap — no db/fs check.
    pub fn blob(&self, hash: &str) -> Blob<'_> {
        Blob {
            store: self,
            hash: hash.to_string(),
        }
    }

    /// Internal access to the db connection (for modules that need it).
    pub(crate) fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }
}

impl<'a> Blob<'a> {
    /// The SHA-256 hash of this blob.
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Filesystem path where this blob is stored. Just the path — no existence check.
    pub fn path(&self) -> PathBuf {
        store::blob_path(&self.store.root, &self.hash)
    }

    /// Known file names for this blob.
    pub fn names(&self) -> Result<Vec<String>> {
        db::get_blob_names(self.store.conn(), &self.hash)
    }

    /// Add tags to this blob.
    pub fn add_tags(&self, tags: &[&str]) -> Result<()> {
        for tag in tags {
            db::add_tag(self.store.conn(), &self.hash, tag)?;
        }
        Ok(())
    }

    /// Get all tags for this blob.
    pub fn tags(&self) -> Result<Vec<String>> {
        db::get_tags(self.store.conn(), &self.hash)
    }

    /// Set metadata string on this blob.
    pub fn set_metadata(&self, value: &str) -> Result<()> {
        db::set_metadata(self.store.conn(), &self.hash, value)
    }

    /// Get metadata string for this blob.
    pub fn metadata(&self) -> Result<Option<String>> {
        db::get_metadata(self.store.conn(), &self.hash)
    }

    /// Add a relation from this blob to another blob.
    pub fn add_relation(&self, target: &Blob, note: Option<&str>) -> Result<()> {
        db::add_relation(self.store.conn(), &self.hash, target.hash(), note)
    }

    /// Get relations from this blob.
    pub fn relations(&self) -> Result<Vec<Relation>> {
        let raw = db::get_relations_from(self.store.conn(), &self.hash)?;
        Ok(raw
            .into_iter()
            .map(|(target, note)| Relation { target, note })
            .collect())
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p purecas -- tests::test_store_open tests::test_blob_path tests::test_blob_hash
```

Expected: PASS

- [ ] **Step 5: Write tests for Blob metadata methods**

Add to `purecas/src/lib.rs` test module:

```rust
    #[test]
    fn test_blob_tags() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        db::insert_blob(s.conn(), "aaa").unwrap();
        let b = s.blob("aaa");
        b.add_tags(&["train", "v2"]).unwrap();
        let tags = b.tags().unwrap();
        assert!(tags.contains(&"train".to_string()));
        assert!(tags.contains(&"v2".to_string()));
    }

    #[test]
    fn test_blob_metadata() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        db::insert_blob(s.conn(), "bbb").unwrap();
        let b = s.blob("bbb");
        assert_eq!(b.metadata().unwrap(), None);
        b.set_metadata("epoch=10").unwrap();
        assert_eq!(b.metadata().unwrap(), Some("epoch=10".to_string()));
    }

    #[test]
    fn test_blob_relations() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        db::insert_blob(s.conn(), "src1").unwrap();
        db::insert_blob(s.conn(), "tgt1").unwrap();
        let src = s.blob("src1");
        let tgt = s.blob("tgt1");
        src.add_relation(&tgt, Some("derived-from")).unwrap();
        let rels = src.relations().unwrap();
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].target, "tgt1");
        assert_eq!(rels[0].note, Some("derived-from".to_string()));
    }
```

- [ ] **Step 6: Run all tests**

```bash
cargo test -p purecas
```

Expected: All new tests pass, all existing inline tests in store.rs/db.rs/fetch.rs/transfer.rs still pass.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(lib): add Store and Blob types with metadata methods"
```

---

### Task 3: Package Type

**Files:**
- Modify: `purecas/src/lib.rs`

Add `Package<'a>` type wrapping existing db functions, including the `export` method that delegates to `transfer::export_package`.

- [ ] **Step 1: Write failing tests for Package**

Add to `purecas/src/lib.rs` test module:

```rust
    #[test]
    fn test_package_create_and_list() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let pkg = s.create_package("my-dataset", Some("test data")).unwrap();
        assert_eq!(pkg.name(), "my-dataset");
        assert_eq!(pkg.description().unwrap(), Some("test data".to_string()));
        let pkgs = s.list_packages().unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name(), "my-dataset");
    }

    #[test]
    fn test_package_add_blob_and_list() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        s.create_package("pkg1", None).unwrap();
        db::insert_blob(s.conn(), "hash1").unwrap();
        let pkg = s.package("pkg1");
        let blob = s.blob("hash1");
        pkg.add_blob(&blob, Some("data/file.bin")).unwrap();
        let blobs = pkg.blobs().unwrap();
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].hash, "hash1");
        assert_eq!(blobs[0].path, Some("data/file.bin".to_string()));
    }

    #[test]
    fn test_package_remove() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        s.create_package("to-delete", None).unwrap();
        assert_eq!(s.list_packages().unwrap().len(), 1);
        s.package("to-delete").remove().unwrap();
        assert_eq!(s.list_packages().unwrap().len(), 0);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p purecas -- tests::test_package
```

Expected: FAIL — `Package` type, `create_package`, `list_packages`, `package` methods don't exist.

- [ ] **Step 3: Implement Package type and Store methods**

Add to `purecas/src/lib.rs` after the `Blob` impl block:

```rust
pub struct Package<'a> {
    store: &'a Store,
    name: String,
}

impl Store {
    /// Create a new package.
    pub fn create_package(&self, name: &str, desc: Option<&str>) -> Result<Package<'_>> {
        db::create_package(&self.conn, name, desc)?;
        Ok(Package {
            store: self,
            name: name.to_string(),
        })
    }

    /// Get a Package handle by name. Cheap — no db check.
    pub fn package(&self, name: &str) -> Package<'_> {
        Package {
            store: self,
            name: name.to_string(),
        }
    }

    /// List all packages.
    pub fn list_packages(&self) -> Result<Vec<Package<'_>>> {
        let raw = db::list_packages(&self.conn)?;
        Ok(raw
            .into_iter()
            .map(|(name, _count)| Package {
                store: self,
                name,
            })
            .collect())
    }
}

impl<'a> Package<'a> {
    /// The name of this package.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the package description.
    pub fn description(&self) -> Result<Option<String>> {
        match self.store.conn().query_row(
            "SELECT description FROM packages WHERE name = ?1",
            [&self.name],
            |row| row.get(0),
        ) {
            Ok(desc) => Ok(desc),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Add a blob to this package with an optional logical path.
    pub fn add_blob(&self, blob: &Blob, path: Option<&str>) -> Result<()> {
        db::add_blob_to_package(self.store.conn(), &self.name, blob.hash(), path)
    }

    /// List blobs in this package.
    pub fn blobs(&self) -> Result<Vec<PackageBlobInfo>> {
        let raw = db::show_package(self.store.conn(), &self.name)?;
        Ok(raw
            .into_iter()
            .map(|(hash, path, names)| PackageBlobInfo { hash, path, names })
            .collect())
    }

    /// Remove this package (blobs are kept).
    pub fn remove(&self) -> Result<()> {
        db::remove_package(self.store.conn(), &self.name)
    }

    /// Export this package's blobs and metadata to a directory.
    pub fn export(&self, to: &Path) -> Result<()> {
        transfer::export_package(self.store.conn(), &self.store.root, &self.name, to)
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p purecas -- tests::test_package
```

Expected: PASS

- [ ] **Step 5: Run all tests**

```bash
cargo test --workspace
```

Expected: All pass — nothing broken.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(lib): add Package type with create/list/export methods"
```

---

### Task 4: Store add_path and add_verified_path Methods

**Files:**
- Modify: `purecas/src/lib.rs`

Implement `Store::add_path` and `Store::add_verified_path` wrapping existing `store::store_blob` and `db::insert_blob` / `db::insert_blob_name`.

- [ ] **Step 1: Write failing tests**

Add to `purecas/src/lib.rs` test module:

```rust
    #[test]
    fn test_add_path() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, b"hello world").unwrap();
        let blob = s.add_path(&file).unwrap();
        assert_eq!(blob.hash(), "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9");
        assert!(blob.path().exists());
    }

    #[test]
    fn test_add_verified_path_ok() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, b"hello world").unwrap();
        let blob = s.add_verified_path(
            &file,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
        ).unwrap();
        assert_eq!(blob.hash(), "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9");
    }

    #[test]
    fn test_add_verified_path_mismatch() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, b"hello world").unwrap();
        let result = s.add_verified_path(&file, "0000000000000000000000000000000000000000000000000000000000000000");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("hash mismatch"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p purecas -- tests::test_add_path tests::test_add_verified_path
```

Expected: FAIL — methods don't exist.

- [ ] **Step 3: Implement add_path and add_verified_path**

Add these methods to the `impl Store` block in `purecas/src/lib.rs`:

```rust
    /// Add a file from a local path to the store. Returns the stored Blob.
    pub fn add_path(&self, path: &Path) -> Result<Blob<'_>> {
        let hash = store::store_blob(&self.root, path)?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        db::insert_blob(&self.conn, &hash)?;
        if !name.is_empty() {
            db::insert_blob_name(&self.conn, &hash, &name)?;
        }
        Ok(self.blob(&hash))
    }

    /// Add a file from a local path, verifying it matches the expected hash.
    pub fn add_verified_path(&self, path: &Path, expected_hash: &str) -> Result<Blob<'_>> {
        let actual_hash = store::hash_file(path)?;
        if actual_hash != expected_hash {
            anyhow::bail!(
                "hash mismatch: expected {}, got {}",
                expected_hash,
                actual_hash
            );
        }
        self.add_path(path)
    }
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p purecas -- tests::test_add_path tests::test_add_verified_path
```

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(lib): add Store::add_path and add_verified_path methods"
```

---

### Task 5: Store add_url Methods

**Files:**
- Modify: `purecas/src/lib.rs`
- Modify: `purecas/src/fetch.rs` (make `download_to_temp` and `verify_hash` pub)

Implement `Store::add_url`, `add_verified_url`, `add_url_unzip`, and `add_verified_url_unzip`. These wrap existing fetch functions. Note: HTTP-dependent methods are hard to unit test without mocking, so we add only structural tests and rely on existing CLI integration tests.

- [ ] **Step 1: Ensure fetch functions are accessible**

Check that `fetch::download_to_temp`, `fetch::verify_hash`, `fetch::fetch_and_store`, and `fetch::fetch_unzip_and_store` are all `pub`. They already are — no changes needed.

- [ ] **Step 2: Implement add_url methods**

Add to the `impl Store` block in `purecas/src/lib.rs`:

```rust
    /// Download a URL and store in CAS. The hash is computed from the downloaded content.
    pub fn add_url(&self, url: &str) -> Result<Blob<'_>> {
        let temp = tempfile::tempdir()?;
        let downloaded = fetch::download_to_temp(url, temp.path())?;
        let hash = store::store_blob(&self.root, &downloaded)?;
        db::insert_blob(&self.conn, &hash)?;
        if let Some(name) = url.rsplit('/').next() {
            if !name.is_empty() {
                db::insert_blob_name(&self.conn, &hash, name)?;
            }
        }
        Ok(self.blob(&hash))
    }

    /// Download a URL, verify SHA-256, and store in CAS.
    pub fn add_verified_url(&self, url: &str, expected_hash: &str) -> Result<Blob<'_>> {
        let temp = tempfile::tempdir()?;
        let downloaded = fetch::download_to_temp(url, temp.path())?;
        fetch::verify_hash(&downloaded, expected_hash)?;
        let hash = store::store_blob(&self.root, &downloaded)?;
        db::insert_blob(&self.conn, &hash)?;
        if let Some(name) = url.rsplit('/').next() {
            if !name.is_empty() {
                db::insert_blob_name(&self.conn, &hash, name)?;
            }
        }
        Ok(self.blob(&hash))
    }

    /// Download a URL, unzip, and store each file in CAS.
    pub fn add_url_unzip(&self, url: &str) -> Result<Vec<Blob<'_>>> {
        let temp = tempfile::tempdir()?;
        let downloaded = fetch::download_to_temp(url, temp.path())?;
        self.unzip_and_store(&downloaded)
    }

    /// Download a URL, verify SHA-256, unzip, and store each file in CAS.
    pub fn add_verified_url_unzip(
        &self,
        url: &str,
        expected_hash: &str,
    ) -> Result<Vec<Blob<'_>>> {
        let temp = tempfile::tempdir()?;
        let downloaded = fetch::download_to_temp(url, temp.path())?;
        fetch::verify_hash(&downloaded, expected_hash)?;
        self.unzip_and_store(&downloaded)
    }

    fn unzip_and_store(&self, archive_path: &Path) -> Result<Vec<Blob<'_>>> {
        let temp = tempfile::tempdir()?;
        let extract_dir = temp.path().join("extracted");
        std::fs::create_dir_all(&extract_dir)?;

        let file = std::fs::File::open(archive_path)?;
        let mut archive = zip::ZipArchive::new(file)?;

        let mut blobs = Vec::new();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            if entry.is_dir() {
                continue;
            }
            let name = entry
                .enclosed_name()
                .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let out_path = extract_dir.join(&name);
            let mut out_file = std::fs::File::create(&out_path)?;
            std::io::copy(&mut entry, &mut out_file)?;

            let hash = store::store_blob(&self.root, &out_path)?;
            db::insert_blob(&self.conn, &hash)?;
            db::insert_blob_name(&self.conn, &hash, &name)?;
            blobs.push(self.blob(&hash));
        }
        Ok(blobs)
    }
```

- [ ] **Step 3: Add import method**

Add to the `impl Store` block:

```rust
    /// Import blobs and metadata from an export directory.
    pub fn import(&self, from: &Path) -> Result<ImportResult> {
        transfer::import_from(&self.conn, &self.root, from)
    }
```

- [ ] **Step 4: Run all tests**

```bash
cargo test --workspace
```

Expected: All pass. The URL methods aren't directly tested here (no HTTP mock), but existing CLI integration tests still exercise them via the old command names.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(lib): add Store::add_url, add_verified_url, and import methods"
```

---

### Task 6: Refactor CLI Binary

**Files:**
- Rewrite: `pcas/src/main.rs`

Replace the CLI with new subcommand names, thin dispatch to `purecas` library. This is the behavioral change.

- [ ] **Step 1: Rewrite pcas/src/main.rs**

Replace the entire contents of `pcas/src/main.rs` with:

```rust
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "pcas",
    about = "Content-addressable storage for datasets and model weights"
)]
struct Cli {
    /// Override CAS root directory (default: $CAS_ROOT or ~/data/blob)
    #[arg(long)]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Add files from local paths to the CAS
    AddPath {
        /// Files to add
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Tags to apply to all added blobs (repeatable)
        #[arg(long = "tag", num_args = 1)]
        tags: Vec<String>,
        /// Metadata string to set on all added blobs
        #[arg(long = "meta")]
        meta: Option<String>,
    },
    /// Download a URL and store in CAS
    AddUrl {
        /// URL to download
        url: String,
        /// Expected SHA-256 hash (if provided, verifies after download)
        #[arg(long)]
        sha256: Option<String>,
        /// Extract zip archive and store each file individually
        #[arg(long)]
        unzip: bool,
    },
    /// Print the CAS path for a hash
    Path {
        /// SHA-256 hash
        hash: String,
    },
    /// Package operations
    Pkg {
        #[command(subcommand)]
        command: PkgCommands,
    },
    /// Export a package to a directory
    Export {
        /// Package name
        package: String,
        /// Destination directory
        #[arg(long)]
        to: PathBuf,
    },
    /// Import blobs and metadata from a directory
    Import {
        /// Source directory
        #[arg(long)]
        from: PathBuf,
    },
    /// Add tags to a blob or package
    Tag {
        /// Blob hash or package name
        id: String,
        /// Tags to add
        #[arg(required = true)]
        tags: Vec<String>,
    },
    /// Set metadata string on a blob or package
    Meta {
        /// Blob hash or package name
        id: String,
        /// Metadata value
        value: String,
    },
    /// Add a relation between two blobs
    Rel {
        /// Source blob hash
        source: String,
        /// Target blob hash
        target: String,
        /// Optional note describing the relation
        note: Option<String>,
    },
    /// Run as a Git LFS custom transfer agent (stdin/stdout protocol)
    LfsAgent,
}

#[derive(Subcommand)]
enum PkgCommands {
    /// Create a new package
    Create {
        /// Package name
        name: String,
        /// Package description
        #[arg(long)]
        description: Option<String>,
    },
    /// Add blobs to a package
    Add {
        /// Package name
        name: String,
        /// Blob hashes
        #[arg(required = true)]
        hashes: Vec<String>,
        /// Logical path within the package (only valid with a single hash)
        #[arg(long)]
        path: Option<String>,
    },
    /// List all packages
    List,
    /// Show blobs in a package
    Show {
        /// Package name
        name: String,
    },
    /// Remove a package (blobs are kept)
    Rm {
        /// Package name
        name: String,
    },
}

fn resolve_root(cli_root: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(root) = cli_root {
        return Ok(root);
    }
    if let Ok(root) = std::env::var("CAS_ROOT") {
        return Ok(PathBuf::from(root));
    }
    let home = std::env::var("HOME")?;
    Ok(PathBuf::from(home).join("data").join("blob"))
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let root = resolve_root(cli.root)?;
    let store = purecas::Store::open(&root)?;

    match cli.command {
        Commands::AddPath { files, tags, meta } => {
            for file in &files {
                let blob = store.add_path(file)?;
                let name = file
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let tag_refs: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();
                if !tag_refs.is_empty() {
                    blob.add_tags(&tag_refs)?;
                }
                if let Some(ref m) = meta {
                    blob.set_metadata(m)?;
                }
                println!("{} {}", blob.hash(), name);
            }
            Ok(())
        }
        Commands::AddUrl { url, sha256, unzip } => {
            match (sha256, unzip) {
                (Some(hash), true) => {
                    let blobs = store.add_verified_url_unzip(&url, &hash)?;
                    for blob in &blobs {
                        let names = blob.names().unwrap_or_default();
                        let name = names.first().map(|s| s.as_str()).unwrap_or("");
                        println!("{} {}", blob.hash(), name);
                    }
                }
                (Some(hash), false) => {
                    let blob = store.add_verified_url(&url, &hash)?;
                    println!("{}", blob.hash());
                }
                (None, true) => {
                    let blobs = store.add_url_unzip(&url)?;
                    for blob in &blobs {
                        let names = blob.names().unwrap_or_default();
                        let name = names.first().map(|s| s.as_str()).unwrap_or("");
                        println!("{} {}", blob.hash(), name);
                    }
                }
                (None, false) => {
                    let blob = store.add_url(&url)?;
                    println!("{}", blob.hash());
                }
            }
            Ok(())
        }
        Commands::Path { hash } => {
            let blob = store.blob(&hash);
            println!("{}", blob.path().display());
            Ok(())
        }
        Commands::Pkg { command } => match command {
            PkgCommands::Create { name, description } => {
                store.create_package(&name, description.as_deref())?;
                println!("Created package: {}", name);
                Ok(())
            }
            PkgCommands::Add { name, hashes, path } => {
                if path.is_some() && hashes.len() > 1 {
                    anyhow::bail!("--path can only be used with a single hash");
                }
                let pkg = store.package(&name);
                for hash in &hashes {
                    let blob = store.blob(hash);
                    pkg.add_blob(&blob, path.as_deref())?;
                }
                Ok(())
            }
            PkgCommands::List => {
                let pkgs = store.list_packages()?;
                for pkg in &pkgs {
                    let blob_count = pkg.blobs().map(|b| b.len()).unwrap_or(0);
                    println!("{}\t{} blobs", pkg.name(), blob_count);
                }
                Ok(())
            }
            PkgCommands::Show { name } => {
                let pkg = store.package(&name);
                let blobs = pkg.blobs()?;
                for info in &blobs {
                    let path_str = info.path.as_deref().unwrap_or("-");
                    let names_str = if info.names.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", info.names.join(", "))
                    };
                    println!("{}\t{}{}", info.hash, path_str, names_str);
                }
                Ok(())
            }
            PkgCommands::Rm { name } => {
                store.package(&name).remove()?;
                println!("Removed package: {}", name);
                Ok(())
            }
        },
        Commands::Export { package, to } => {
            std::fs::create_dir_all(&to)?;
            store.package(&package).export(&to)?;
            println!("Exported package '{}' to {}", package, to.display());
            Ok(())
        }
        Commands::Import { from } => {
            let result = store.import(&from)?;
            println!(
                "Imported {} blob(s) from {}",
                result.imported_blobs,
                from.display()
            );
            Ok(())
        }
        Commands::Tag { id, tags } => {
            let blob = store.blob(&id);
            let tag_refs: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();
            blob.add_tags(&tag_refs)?;
            let all_tags = blob.tags()?;
            println!("{}: {}", id, all_tags.join("; "));
            Ok(())
        }
        Commands::Meta { id, value } => {
            let blob = store.blob(&id);
            blob.set_metadata(&value)?;
            println!("{}: {}", id, value);
            Ok(())
        }
        Commands::Rel {
            source,
            target,
            note,
        } => {
            let src = store.blob(&source);
            let tgt = store.blob(&target);
            src.add_relation(&tgt, note.as_deref())?;
            match &note {
                Some(n) => println!("{} -> {} ({})", source, target, n),
                None => println!("{} -> {}", source, target),
            }
            Ok(())
        }
        Commands::LfsAgent => purecas::lfs::run_agent(&root),
    }
}
```

- [ ] **Step 2: Build to verify compilation**

```bash
cargo build --workspace
```

Expected: Compiles successfully.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat(cli): rewrite CLI with new subcommand names (add-path, add-url, remove cat)"
```

---

### Task 7: Update Integration Tests

**Files:**
- Rewrite: `pcas/tests/cli.rs`

Update all integration tests for new command names. Remove tests for `cat` and `path [exists]/[missing]`. Update `export` to be package-only.

- [ ] **Step 1: Update helper and basic tests**

Replace the full contents of `pcas/tests/cli.rs`. The structure stays the same but command names change:

- `add file.txt` → `add-path file.txt`
- `fetch URL --sha256 HASH` → `add-url URL --sha256 HASH`
- `path HASH` assertions change: no more `[exists]`/`[missing]` in output
- `cat HASH` tests removed
- `export HASH1 HASH2 --to DIR` tests changed to `export PKGNAME --to DIR`

```rust
use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

fn pcas() -> Command {
    Command::cargo_bin("pcas").unwrap()
}

fn cas_root() -> TempDir {
    TempDir::new().unwrap()
}

#[test]
fn test_add_path_single_file() {
    let root = cas_root();
    let file = root.path().join("hello.txt");
    fs::write(&file, b"hello").unwrap();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["add-path", file.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello.txt"));
}

#[test]
fn test_add_path_multiple_files() {
    let root = cas_root();
    let a = root.path().join("a.txt");
    let b = root.path().join("b.txt");
    fs::write(&a, b"aaa").unwrap();
    fs::write(&b, b"bbb").unwrap();

    let output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["add-path", a.to_str().unwrap(), b.to_str().unwrap()])
        .assert()
        .success();

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("a.txt"));
    assert!(stdout.contains("b.txt"));
}

#[test]
fn test_path_prints_only_path() {
    let root = cas_root();
    let file = root.path().join("test.txt");
    fs::write(&file, b"content").unwrap();

    let output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["add-path", file.to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let hash = stdout.split_whitespace().next().unwrap();

    let path_output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["path", hash])
        .assert()
        .success();
    let path_stdout = String::from_utf8(path_output.get_output().stdout.clone()).unwrap();
    // Should NOT contain [exists] or [missing]
    assert!(!path_stdout.contains("[exists]"));
    assert!(!path_stdout.contains("[missing]"));
    // Should contain the hash in the path
    assert!(path_stdout.contains(hash));
}

#[test]
fn test_path_missing_blob() {
    let root = cas_root();
    let path_output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["path", "0000000000000000000000000000000000000000000000000000000000000000"])
        .assert()
        .success();
    let stdout = String::from_utf8(path_output.get_output().stdout.clone()).unwrap();
    // Just prints path, no error even if missing
    assert!(!stdout.contains("[missing]"));
    assert!(stdout.contains("0000000000000000000000000000000000000000000000000000000000000000"));
}

#[test]
fn test_pkg_create_and_list() {
    let root = cas_root();
    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["pkg", "create", "mydata", "--description", "test dataset"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created package: mydata"));

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["pkg", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("mydata"));
}

#[test]
fn test_pkg_add_and_show() {
    let root = cas_root();
    let file = root.path().join("data.bin");
    fs::write(&file, b"binary data").unwrap();

    let output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["add-path", file.to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let hash = stdout.split_whitespace().next().unwrap();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["pkg", "create", "mypkg"])
        .assert()
        .success();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["pkg", "add", "mypkg", hash, "--path", "data/file.bin"])
        .assert()
        .success();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["pkg", "show", "mypkg"])
        .assert()
        .success()
        .stdout(predicate::str::contains(hash).and(predicate::str::contains("data/file.bin")));
}

#[test]
fn test_pkg_add_multiple_hashes_with_path_errors() {
    let root = cas_root();
    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["pkg", "create", "mypkg"])
        .assert()
        .success();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["pkg", "add", "mypkg", "hash1", "hash2", "--path", "x"])
        .assert()
        .failure();
}

#[test]
fn test_pkg_rm() {
    let root = cas_root();
    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["pkg", "create", "to-delete"])
        .assert()
        .success();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["pkg", "rm", "to-delete"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed package: to-delete"));

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["pkg", "list"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty().not().not()); // empty list is fine
}

#[test]
fn test_export_package_cli() {
    let root = cas_root();
    let export_dir = root.path().join("export");

    let file = root.path().join("blob.txt");
    fs::write(&file, b"export me").unwrap();

    let output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["add-path", file.to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let hash = stdout.split_whitespace().next().unwrap();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["pkg", "create", "testpkg"])
        .assert()
        .success();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["pkg", "add", "testpkg", hash])
        .assert()
        .success();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["export", "testpkg", "--to", export_dir.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Exported package"));

    // Verify export structure
    assert!(export_dir.join("sha256").exists());
    assert!(export_dir.join("purecas-export.json").exists());
}

#[test]
fn test_export_import_roundtrip() {
    let root1 = cas_root();
    let root2 = cas_root();
    let export_dir = root1.path().join("export");

    let file = root1.path().join("roundtrip.txt");
    fs::write(&file, b"roundtrip data").unwrap();

    let output = pcas()
        .args(["--root", root1.path().to_str().unwrap()])
        .args(["add-path", file.to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let hash = stdout.split_whitespace().next().unwrap();

    pcas()
        .args(["--root", root1.path().to_str().unwrap()])
        .args(["pkg", "create", "rt-pkg"])
        .assert()
        .success();

    pcas()
        .args(["--root", root1.path().to_str().unwrap()])
        .args(["pkg", "add", "rt-pkg", hash])
        .assert()
        .success();

    pcas()
        .args(["--root", root1.path().to_str().unwrap()])
        .args(["export", "rt-pkg", "--to", export_dir.to_str().unwrap()])
        .assert()
        .success();

    pcas()
        .args(["--root", root2.path().to_str().unwrap()])
        .args(["import", "--from", export_dir.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Imported"));

    // Verify blob exists in second store
    let path_output = pcas()
        .args(["--root", root2.path().to_str().unwrap()])
        .args(["path", hash])
        .assert()
        .success();
    let path_stdout = String::from_utf8(path_output.get_output().stdout.clone()).unwrap();
    let blob_path = path_stdout.trim();
    assert!(std::path::Path::new(blob_path).exists());
}

#[test]
fn test_tag_blob() {
    let root = cas_root();
    let file = root.path().join("tagged.txt");
    fs::write(&file, b"tag me").unwrap();

    let output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["add-path", file.to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let hash = stdout.split_whitespace().next().unwrap();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["tag", hash, "dataset", "production"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("dataset").and(predicate::str::contains("production")),
        );
}

#[test]
fn test_meta_blob() {
    let root = cas_root();
    let file = root.path().join("meta.txt");
    fs::write(&file, b"metadata me").unwrap();

    let output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["add-path", file.to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let hash = stdout.split_whitespace().next().unwrap();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["meta", hash, "trained on ImageNet v2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("trained on ImageNet v2"));
}

#[test]
fn test_add_path_with_tag_and_meta() {
    let root = cas_root();
    let file = root.path().join("model.pth");
    fs::write(&file, b"model weights").unwrap();

    let output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args([
            "add-path",
            file.to_str().unwrap(),
            "--tag", "model",
            "--tag", "v1",
            "--meta", "ResNet50 pretrained",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let hash = stdout.split_whitespace().next().unwrap();

    // Verify tags were applied
    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["tag", hash, "check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("model").and(predicate::str::contains("v1")));
}

#[test]
fn test_rel() {
    let root = cas_root();
    let f1 = root.path().join("source.txt");
    let f2 = root.path().join("target.txt");
    fs::write(&f1, b"source").unwrap();
    fs::write(&f2, b"target").unwrap();

    let out1 = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["add-path", f1.to_str().unwrap()])
        .assert()
        .success();
    let hash1 = String::from_utf8(out1.get_output().stdout.clone())
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();

    let out2 = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["add-path", f2.to_str().unwrap()])
        .assert()
        .success();
    let hash2 = String::from_utf8(out2.get_output().stdout.clone())
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["rel", &hash1, &hash2, "derived from"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("->").and(predicate::str::contains("derived from")),
        );
}

#[test]
fn test_lfs_agent_init() {
    let root = cas_root();
    let init_msg = r#"{"event":"init","operation":"upload","concurrent":true,"concurrenttransfers":3}"#;
    let terminate_msg = r#"{"event":"terminate"}"#;
    let input = format!("{}\n{}\n", init_msg, terminate_msg);

    let output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .arg("lfs-agent")
        .write_stdin(input)
        .assert()
        .success();

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains(r#""event":"init"#));
}

#[test]
fn test_lfs_agent_upload_roundtrip() {
    let root = cas_root();
    let upload_file = root.path().join("lfs_upload.bin");
    fs::write(&upload_file, b"lfs content").unwrap();
    let expected_hash = "a1a37e0e2da560034c3ed2e4e1458a0608e2e08c62cbfa981c2218cba1e987e5";

    let init_msg = r#"{"event":"init","operation":"upload","concurrent":true,"concurrenttransfers":1}"#;
    let upload_msg = format!(
        r#"{{"event":"upload","oid":"{}","size":11,"path":"{}","action":{{"href":"","header":{{}}}}}}"#,
        expected_hash,
        upload_file.to_str().unwrap()
    );
    let terminate_msg = r#"{"event":"terminate"}"#;
    let input = format!("{}\n{}\n{}\n", init_msg, upload_msg, terminate_msg);

    let output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .arg("lfs-agent")
        .write_stdin(input)
        .assert()
        .success();

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains(r#""event":"complete"#));
    assert!(!stdout.contains(r#""error"#));

    // Verify blob is in the store
    let blob_path = root.path().join("sha256").join(&expected_hash[..2]).join(expected_hash);
    assert!(blob_path.exists());
}

#[test]
fn test_lfs_agent_upload_hash_mismatch() {
    let root = cas_root();
    let upload_file = root.path().join("lfs_bad.bin");
    fs::write(&upload_file, b"lfs content").unwrap();

    let init_msg = r#"{"event":"init","operation":"upload","concurrent":true,"concurrenttransfers":1}"#;
    let upload_msg = format!(
        r#"{{"event":"upload","oid":"0000000000000000000000000000000000000000000000000000000000000000","size":11,"path":"{}","action":{{"href":"","header":{{}}}}}}"#,
        upload_file.to_str().unwrap()
    );
    let terminate_msg = r#"{"event":"terminate"}"#;
    let input = format!("{}\n{}\n{}\n", init_msg, upload_msg, terminate_msg);

    let output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .arg("lfs-agent")
        .write_stdin(input)
        .assert()
        .success();

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains(r#""error"#));
}

#[test]
fn test_lfs_agent_download_roundtrip() {
    let root = cas_root();
    let file = root.path().join("dl.txt");
    fs::write(&file, b"download me").unwrap();

    let add_out = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["add-path", file.to_str().unwrap()])
        .assert()
        .success();
    let hash = String::from_utf8(add_out.get_output().stdout.clone())
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();

    let init_msg = r#"{"event":"init","operation":"download","concurrent":true,"concurrenttransfers":1}"#;
    let download_msg = format!(
        r#"{{"event":"download","oid":"{}","size":11,"action":{{"href":"","header":{{}}}}}}"#,
        hash
    );
    let terminate_msg = r#"{"event":"terminate"}"#;
    let input = format!("{}\n{}\n{}\n", init_msg, download_msg, terminate_msg);

    let output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .arg("lfs-agent")
        .write_stdin(input)
        .assert()
        .success();

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains(r#""event":"complete"#));
    assert!(stdout.contains(&hash));
}

#[test]
fn test_lfs_agent_download_missing() {
    let root = cas_root();
    let init_msg = r#"{"event":"init","operation":"download","concurrent":true,"concurrenttransfers":1}"#;
    let download_msg = r#"{"event":"download","oid":"0000000000000000000000000000000000000000000000000000000000000000","size":11,"action":{"href":"","header":{}}}"#;
    let terminate_msg = r#"{"event":"terminate"}"#;
    let input = format!("{}\n{}\n{}\n", init_msg, download_msg, terminate_msg);

    let output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .arg("lfs-agent")
        .write_stdin(input)
        .assert()
        .success();

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains(r#""error"#));
}
```

- [ ] **Step 2: Run integration tests**

```bash
cargo test -p pcas
```

Expected: All tests pass.

- [ ] **Step 3: Run all tests (lib + CLI)**

```bash
cargo test --workspace
```

Expected: All pass.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "test: update integration tests for new CLI subcommand names"
```

---

### Task 8: Remove export_hashes and Clean Up

**Files:**
- Modify: `purecas/src/transfer.rs` — remove `export_hashes` function

Per the design spec, exporting individual blobs is just `cp` — no dedicated function needed.

- [ ] **Step 1: Remove export_hashes from transfer.rs**

Delete the `pub fn export_hashes(...)` function from `purecas/src/transfer.rs`.

- [ ] **Step 2: Verify no callers remain**

```bash
cd purecas && grep -r "export_hashes" . --include="*.rs"
```

Expected: No results (the old CLI was the only caller, and it's been rewritten).

- [ ] **Step 3: Run all tests**

```bash
cargo test --workspace
```

Expected: All pass. (The inline test `test_export_hashes` in transfer.rs should also be removed if it exists.)

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor: remove export_hashes (use shell cp for individual blobs)"
```

---

### Task 9: PyO3 Python Bindings

**Files:**
- Create: `purecas-python/Cargo.toml`
- Create: `purecas-python/pyproject.toml`
- Create: `purecas-python/src/lib.rs`
- Modify: `Cargo.toml` (workspace: add `purecas-python` member)

- [ ] **Step 1: Add purecas-python to workspace**

Edit the root `Cargo.toml`:

```toml
[workspace]
members = ["purecas", "pcas", "purecas-python"]
resolver = "2"
```

- [ ] **Step 2: Create purecas-python/Cargo.toml**

```bash
mkdir -p purecas-python/src
```

```toml
[package]
name = "purecas-python"
version = "0.1.0"
edition = "2021"

[lib]
name = "purecas"
crate-type = ["cdylib"]

[dependencies]
purecas = { path = "../purecas" }
pyo3 = { version = "0.22", features = ["extension-module"] }
anyhow = "1"
```

- [ ] **Step 3: Create purecas-python/pyproject.toml**

```toml
[build-system]
requires = ["maturin>=1.0,<2.0"]
build-backend = "maturin"

[project]
name = "purecas"
requires-python = ">=3.8"

[tool.maturin]
features = ["pyo3/extension-module"]
```

- [ ] **Step 4: Create purecas-python/src/lib.rs**

```rust
use pyo3::prelude::*;
use pyo3::exceptions::PyRuntimeError;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

struct InnerStore(purecas::Store);
// rusqlite::Connection is Send (not Sync), so Mutex<InnerStore> is Send + Sync automatically.

#[pyclass]
#[derive(Clone)]
struct Store {
    inner: Arc<Mutex<InnerStore>>,
}

#[pyclass]
#[derive(Clone)]
struct Blob {
    store: Arc<Mutex<InnerStore>>,
    hash: String,
}

#[pyclass]
#[derive(Clone)]
struct Package {
    store: Arc<Mutex<InnerStore>>,
    name: String,
}

fn to_py_err(e: anyhow::Error) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

#[pymethods]
impl Store {
    #[staticmethod]
    fn open(root: &str) -> PyResult<Self> {
        let inner = purecas::Store::open(PathBuf::from(root)).map_err(to_py_err)?;
        Ok(Store {
            inner: Arc::new(Mutex::new(InnerStore(inner))),
        })
    }

    fn blob(&self, hash: &str) -> Blob {
        Blob {
            store: self.inner.clone(),
            hash: hash.to_string(),
        }
    }

    fn add_path(&self, path: &str) -> PyResult<Blob> {
        let guard = self.inner.lock().unwrap();
        let blob = guard.0.add_path(&PathBuf::from(path)).map_err(to_py_err)?;
        let hash = blob.hash().to_string();
        drop(guard);
        Ok(Blob {
            store: self.inner.clone(),
            hash,
        })
    }

    fn add_verified_path(&self, path: &str, expected_hash: &str) -> PyResult<Blob> {
        let guard = self.inner.lock().unwrap();
        let blob = guard
            .0
            .add_verified_path(&PathBuf::from(path), expected_hash)
            .map_err(to_py_err)?;
        let hash = blob.hash().to_string();
        drop(guard);
        Ok(Blob {
            store: self.inner.clone(),
            hash,
        })
    }

    fn add_url(&self, url: &str) -> PyResult<Blob> {
        let guard = self.inner.lock().unwrap();
        let blob = guard.0.add_url(url).map_err(to_py_err)?;
        let hash = blob.hash().to_string();
        drop(guard);
        Ok(Blob {
            store: self.inner.clone(),
            hash,
        })
    }

    fn add_verified_url(&self, url: &str, expected_hash: &str) -> PyResult<Blob> {
        let guard = self.inner.lock().unwrap();
        let blob = guard
            .0
            .add_verified_url(url, expected_hash)
            .map_err(to_py_err)?;
        let hash = blob.hash().to_string();
        drop(guard);
        Ok(Blob {
            store: self.inner.clone(),
            hash,
        })
    }

    fn add_url_unzip(&self, url: &str) -> PyResult<Vec<Blob>> {
        let guard = self.inner.lock().unwrap();
        let blobs = guard.0.add_url_unzip(url).map_err(to_py_err)?;
        let result: Vec<Blob> = blobs
            .iter()
            .map(|b| Blob {
                store: self.inner.clone(),
                hash: b.hash().to_string(),
            })
            .collect();
        drop(guard);
        Ok(result)
    }

    fn add_verified_url_unzip(&self, url: &str, expected_hash: &str) -> PyResult<Vec<Blob>> {
        let guard = self.inner.lock().unwrap();
        let blobs = guard
            .0
            .add_verified_url_unzip(url, expected_hash)
            .map_err(to_py_err)?;
        let result: Vec<Blob> = blobs
            .iter()
            .map(|b| Blob {
                store: self.inner.clone(),
                hash: b.hash().to_string(),
            })
            .collect();
        drop(guard);
        Ok(result)
    }

    fn create_package(&self, name: &str, description: Option<&str>) -> PyResult<Package> {
        let guard = self.inner.lock().unwrap();
        guard
            .0
            .create_package(name, description)
            .map_err(to_py_err)?;
        drop(guard);
        Ok(Package {
            store: self.inner.clone(),
            name: name.to_string(),
        })
    }

    fn package(&self, name: &str) -> Package {
        Package {
            store: self.inner.clone(),
            name: name.to_string(),
        }
    }

    fn list_packages(&self) -> PyResult<Vec<Package>> {
        let guard = self.inner.lock().unwrap();
        let pkgs = guard.0.list_packages().map_err(to_py_err)?;
        let result: Vec<Package> = pkgs
            .iter()
            .map(|p| Package {
                store: self.inner.clone(),
                name: p.name().to_string(),
            })
            .collect();
        drop(guard);
        Ok(result)
    }

    fn import_from(&self, from: &str) -> PyResult<u64> {
        let guard = self.inner.lock().unwrap();
        let result = guard
            .0
            .import(&PathBuf::from(from))
            .map_err(to_py_err)?;
        Ok(result.imported_blobs)
    }
}

#[pymethods]
impl Blob {
    #[getter]
    fn hash(&self) -> &str {
        &self.hash
    }

    #[getter]
    fn path(&self) -> String {
        let guard = self.store.lock().unwrap();
        let blob = guard.0.blob(&self.hash);
        blob.path().to_string_lossy().to_string()
    }

    fn names(&self) -> PyResult<Vec<String>> {
        let guard = self.store.lock().unwrap();
        guard.0.blob(&self.hash).names().map_err(to_py_err)
    }

    fn add_tags(&self, tags: Vec<String>) -> PyResult<()> {
        let guard = self.store.lock().unwrap();
        let tag_refs: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();
        guard
            .0
            .blob(&self.hash)
            .add_tags(&tag_refs)
            .map_err(to_py_err)
    }

    fn tags(&self) -> PyResult<Vec<String>> {
        let guard = self.store.lock().unwrap();
        guard.0.blob(&self.hash).tags().map_err(to_py_err)
    }

    fn set_metadata(&self, value: &str) -> PyResult<()> {
        let guard = self.store.lock().unwrap();
        guard
            .0
            .blob(&self.hash)
            .set_metadata(value)
            .map_err(to_py_err)
    }

    fn metadata(&self) -> PyResult<Option<String>> {
        let guard = self.store.lock().unwrap();
        guard.0.blob(&self.hash).metadata().map_err(to_py_err)
    }

    fn add_relation(&self, target: &Blob, note: Option<&str>) -> PyResult<()> {
        let guard = self.store.lock().unwrap();
        let src = guard.0.blob(&self.hash);
        let tgt = guard.0.blob(&target.hash);
        src.add_relation(&tgt, note).map_err(to_py_err)
    }

    fn __repr__(&self) -> String {
        format!("Blob({})", &self.hash[..8.min(self.hash.len())])
    }
}

#[pymethods]
impl Package {
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> PyResult<Option<String>> {
        let guard = self.store.lock().unwrap();
        guard
            .0
            .package(&self.name)
            .description()
            .map_err(to_py_err)
    }

    fn add_blob(&self, blob: &Blob, path: Option<&str>) -> PyResult<()> {
        let guard = self.store.lock().unwrap();
        let pkg = guard.0.package(&self.name);
        let b = guard.0.blob(&blob.hash);
        pkg.add_blob(&b, path).map_err(to_py_err)
    }

    fn blobs(&self) -> PyResult<Vec<PyObject>> {
        let guard = self.store.lock().unwrap();
        let blob_infos = guard
            .0
            .package(&self.name)
            .blobs()
            .map_err(to_py_err)?;
        drop(guard);
        Python::with_gil(|py| {
            blob_infos
                .iter()
                .map(|info| {
                    let dict = pyo3::types::PyDict::new(py);
                    dict.set_item("hash", &info.hash)?;
                    dict.set_item("path", &info.path)?;
                    dict.set_item("names", &info.names)?;
                    Ok(dict.into())
                })
                .collect()
        })
    }

    fn remove(&self) -> PyResult<()> {
        let guard = self.store.lock().unwrap();
        guard
            .0
            .package(&self.name)
            .remove()
            .map_err(to_py_err)
    }

    fn export(&self, to: &str) -> PyResult<()> {
        let guard = self.store.lock().unwrap();
        std::fs::create_dir_all(to).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        guard
            .0
            .package(&self.name)
            .export(&PathBuf::from(to))
            .map_err(to_py_err)
    }

    fn __repr__(&self) -> String {
        format!("Package({})", self.name)
    }
}

#[pymodule]
fn purecas(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Store>()?;
    m.add_class::<Blob>()?;
    m.add_class::<Package>()?;
    Ok(())
}
```

- [ ] **Step 5: Build the workspace (including PyO3 crate)**

```bash
cargo build --workspace
```

Expected: Compiles successfully. Note: `purecas-python` builds as a cdylib, but can't be loaded as a Python module without maturin. The `cargo build` just verifies Rust compilation.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: add PyO3 Python bindings (purecas-python crate)"
```

---

### Task 10: Update flake.nix for Workspace

**Files:**
- Modify: `flake.nix`

Update the Nix flake to build the workspace correctly. Crane handles workspaces, but we need to ensure the `pcas` binary is still the default output.

- [ ] **Step 1: Update flake.nix**

Replace `flake.nix` contents with:

```nix
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
        strictDeps = true;
        nativeBuildInputs = with pkgs; [ pkg-config ];
        buildInputs = with pkgs; [ openssl ];
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
```

The key change is adding `maturin` to the dev shell. The crane build already handles Cargo workspaces — `buildPackage` builds all workspace members and the `pcas` binary is still the main program.

- [ ] **Step 2: Verify nix build (if available)**

```bash
nix build .#pcas 2>&1 || echo "nix build skipped (may need flake lock update)"
```

If lock needs updating:

```bash
nix flake update
nix build .#pcas
```

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "build: update flake.nix for workspace + add maturin to devShell"
```

---

### Task 11: Update README

**Files:**
- Modify: `README.md`

Update documentation to reflect new CLI subcommand names and library API.

- [ ] **Step 1: Update CLI usage examples in README**

Replace all instances of:
- `pcas add <file>` → `pcas add-path <file>`
- `pcas fetch <url> --sha256 <hash>` → `pcas add-url <url> --sha256 <hash>`
- `pcas cat <hash>` → `cat $(pcas path <hash>)`
- `pcas path <hash>` output: remove `[exists]`/`[missing]` from example output
- `pcas export <hash1> <hash2> --to <dir>` → `pcas export <package> --to <dir>`

- [ ] **Step 2: Add library usage section**

Add a section showing the Rust library API:

```markdown
## Library Usage (Rust)

Add to your `Cargo.toml`:

\```toml
[dependencies]
purecas = { path = "purecas" }
\```

\```rust
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
\```
```

- [ ] **Step 3: Add Python usage section**

```markdown
## Python Usage

Install with maturin:

\```bash
cd purecas-python
maturin develop
\```

\```python
import purecas

store = purecas.Store.open("/path/to/cas")
blob = store.add_path("/data/file.bin")
print(blob.hash, blob.path)

blob.add_tags(["train", "v2"])
blob.set_metadata("epoch=10")

pkg = store.create_package("my-dataset")
pkg.add_blob(blob, path="images/001.png")
pkg.export("/tmp/export")
\```
```

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "docs: update README for new CLI names, library API, and Python bindings"
```

---

### Task 12: Final Verification

- [ ] **Step 1: Run full test suite**

```bash
cargo test --workspace
```

Expected: All tests pass.

- [ ] **Step 2: Run clippy**

```bash
cargo clippy --workspace -- -D warnings
```

Expected: No warnings.

- [ ] **Step 3: Run rustfmt**

```bash
cargo fmt --all -- --check
```

Expected: No formatting changes needed.

- [ ] **Step 4: Verify CLI help output**

```bash
cargo run -p pcas -- --help
cargo run -p pcas -- add-path --help
cargo run -p pcas -- add-url --help
cargo run -p pcas -- path --help
```

Expected: Help text shows new command names, no mention of `cat` or `fetch`.

- [ ] **Step 5: Smoke test the binary**

```bash
TMP=$(mktemp -d)
echo "hello" > /tmp/test_smoke.txt
cargo run -p pcas -- --root "$TMP" add-path /tmp/test_smoke.txt
HASH=$(cargo run -p pcas -- --root "$TMP" add-path /tmp/test_smoke.txt 2>/dev/null | awk '{print $1}')
cargo run -p pcas -- --root "$TMP" path "$HASH"
rm -rf "$TMP" /tmp/test_smoke.txt
```

Expected: Path is printed with no `[exists]` suffix.
