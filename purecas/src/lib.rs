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

impl<'a> std::fmt::Debug for Blob<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Blob").field("hash", &self.hash).finish()
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
    pub fn add_verified_url_unzip(&self, url: &str, expected_hash: &str) -> Result<Vec<Blob<'_>>> {
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

    /// Import blobs and metadata from an export directory.
    pub fn import(&self, from: &Path) -> Result<ImportResult> {
        transfer::import_from(&self.conn, &self.root, from)
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
            .map(|(name, _count)| Package { store: self, name })
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
        let expected = dir
            .path()
            .join("sha256")
            .join("ab")
            .join("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        assert_eq!(b.path(), expected);
    }

    #[test]
    fn test_blob_hash() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let b = s.blob("abc123");
        assert_eq!(b.hash(), "abc123");
    }

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

    #[test]
    fn test_add_path() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, b"hello world").unwrap();
        let blob = s.add_path(&file).unwrap();
        assert_eq!(
            blob.hash(),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert!(blob.path().exists());
    }

    #[test]
    fn test_add_verified_path_ok() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, b"hello world").unwrap();
        let blob = s
            .add_verified_path(
                &file,
                "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
            )
            .unwrap();
        assert_eq!(
            blob.hash(),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_add_verified_path_mismatch() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, b"hello world").unwrap();
        let result = s.add_verified_path(
            &file,
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("hash mismatch"));
    }

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
}
