// Blob storage: hashing, storing, path resolution, reading

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Compute the CAS path for a given hash under a root.
pub fn blob_path(root: &Path, hash: &str) -> PathBuf {
    root.join("sha256").join(&hash[..2]).join(hash)
}

/// Check if a blob exists on disk.
pub fn blob_exists(root: &Path, hash: &str) -> bool {
    blob_path(root, hash).exists()
}

/// Compute SHA-256 hash of a file, returned as lowercase hex string.
pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Store a file in the CAS. Returns the SHA-256 hash.
/// If the blob already exists, this is a no-op.
/// The stored file is set to read-only (0o444).
pub fn store_blob(root: &Path, source: &Path) -> Result<String> {
    let hash = hash_file(source)?;
    let dest = blob_path(root, &hash);
    if dest.exists() {
        return Ok(hash);
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, &dest).with_context(|| format!("copying {} to CAS", source.display()))?;
    let perms = fs::Permissions::from_mode(0o444);
    fs::set_permissions(&dest, perms)?;
    Ok(hash)
}

/// Read a blob from the CAS by hash.
pub fn read_blob(root: &Path, hash: &str) -> Result<Vec<u8>> {
    let path = blob_path(root, hash);
    fs::read(&path).with_context(|| format!("reading blob {}", hash))
}

/// Write a blob to stdout.
pub fn cat_blob(root: &Path, hash: &str) -> Result<()> {
    let data = read_blob(root, hash)?;
    io::stdout().write_all(&data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_blob_path() {
        let root = Path::new("/data/blob");
        let hash = "a3f2c1deadbeef0123456789abcdef0123456789abcdef0123456789abcdef01";
        let p = blob_path(root, hash);
        assert_eq!(
            p,
            PathBuf::from("/data/blob/sha256/a3/a3f2c1deadbeef0123456789abcdef0123456789abcdef0123456789abcdef01")
        );
    }

    #[test]
    fn test_hash_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("hello.txt");
        fs::write(&file, b"hello world").unwrap();
        let hash = hash_file(&file).unwrap();
        assert_eq!(hash, "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9");
    }

    #[test]
    fn test_store_and_read_blob() {
        let cas_root = TempDir::new().unwrap();
        let src_dir = TempDir::new().unwrap();
        let file = src_dir.path().join("test.bin");
        fs::write(&file, b"test content").unwrap();
        let hash = store_blob(cas_root.path(), &file).unwrap();
        assert!(blob_exists(cas_root.path(), &hash));
        let stored = blob_path(cas_root.path(), &hash);
        let perms = fs::metadata(&stored).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o444);
        let data = read_blob(cas_root.path(), &hash).unwrap();
        assert_eq!(data, b"test content");
    }

    #[test]
    fn test_store_blob_idempotent() {
        let cas_root = TempDir::new().unwrap();
        let src_dir = TempDir::new().unwrap();
        let file = src_dir.path().join("test.bin");
        fs::write(&file, b"same content").unwrap();
        let hash1 = store_blob(cas_root.path(), &file).unwrap();
        let hash2 = store_blob(cas_root.path(), &file).unwrap();
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_read_blob_missing() {
        let cas_root = TempDir::new().unwrap();
        let result = read_blob(cas_root.path(), "0000000000000000000000000000000000000000000000000000000000000000");
        assert!(result.is_err());
    }
}
