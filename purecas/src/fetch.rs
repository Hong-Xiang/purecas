// HTTP download, SHA-256 verification, optional unzip
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

use crate::store;

/// Download a URL to a temp file in the given directory. Returns the temp file path.
pub fn download_to_temp(url: &str, temp_dir: &Path) -> Result<std::path::PathBuf> {
    let response = reqwest::blocking::get(url).with_context(|| format!("downloading {}", url))?;
    if !response.status().is_success() {
        bail!("HTTP {} for {}", response.status(), url);
    }
    let bytes = response.bytes()?;
    let temp_path = temp_dir.join("download");
    fs::write(&temp_path, &bytes)?;
    Ok(temp_path)
}

/// Verify a file's SHA-256 matches the expected hash.
pub fn verify_hash(path: &Path, expected: &str) -> Result<()> {
    let actual = store::hash_file(path)?;
    if actual != expected {
        bail!("hash mismatch: expected {}, got {}", expected, actual);
    }
    Ok(())
}

/// Fetch a URL, verify SHA-256, store in CAS. Returns the hash.
pub fn fetch_and_store(url: &str, expected_hash: &str, root: &Path) -> Result<String> {
    let temp = tempfile::tempdir()?;
    let downloaded = download_to_temp(url, temp.path())?;
    verify_hash(&downloaded, expected_hash)?;
    let hash = store::store_blob(root, &downloaded)?;
    Ok(hash)
}

/// Fetch a URL, verify SHA-256, unzip, store each file. Returns vec of (hash, filename).
pub fn fetch_unzip_and_store(
    url: &str,
    expected_hash: &str,
    root: &Path,
) -> Result<Vec<(String, String)>> {
    let temp = tempfile::tempdir()?;
    let downloaded = download_to_temp(url, temp.path())?;
    verify_hash(&downloaded, expected_hash)?;

    let extract_dir = temp.path().join("extracted");
    fs::create_dir_all(&extract_dir)?;

    let file = fs::File::open(&downloaded)?;
    let mut archive = zip::ZipArchive::new(file)?;

    let mut results = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry
            .enclosed_name()
            .map(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            })
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let out_path = extract_dir.join(&name);
        let mut out_file = fs::File::create(&out_path)?;
        std::io::copy(&mut entry, &mut out_file)?;

        let hash = store::store_blob(root, &out_path)?;
        results.push((hash, name));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_verify_hash_ok() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, b"hello world").unwrap();
        verify_hash(
            &file,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
        )
        .unwrap();
    }

    #[test]
    fn test_verify_hash_mismatch() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, b"hello world").unwrap();
        let result = verify_hash(
            &file,
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("hash mismatch"));
    }
}
