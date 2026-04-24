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
}
