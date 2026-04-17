use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::{db, store};

#[derive(Serialize, Deserialize)]
pub struct ExportMetadata {
    pub packages: Vec<ExportPackage>,
    pub blob_names: HashMap<String, Vec<String>>,
}

#[derive(Serialize, Deserialize)]
pub struct ExportPackage {
    pub name: String,
    pub description: Option<String>,
    pub blobs: Vec<ExportBlob>,
}

#[derive(Serialize, Deserialize)]
pub struct ExportBlob {
    pub hash: String,
    pub path: Option<String>,
}

const EXPORT_FILENAME: &str = "purecas-export.json";

/// Export blobs for a package to a target directory.
pub fn export_package(conn: &Connection, root: &Path, package: &str, to: &Path) -> Result<()> {
    let blobs = db::show_package(conn, package)?;
    let hashes: Vec<String> = blobs.iter().map(|(h, _, _)| h.clone()).collect();

    for hash in &hashes {
        copy_blob_to(root, hash, to)?;
    }

    let description: Option<String> = conn
        .query_row(
            "SELECT description FROM packages WHERE name = ?1",
            [package],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    let export_blobs: Vec<ExportBlob> = blobs
        .iter()
        .map(|(hash, path, _)| ExportBlob {
            hash: hash.clone(),
            path: path.clone(),
        })
        .collect();

    let blob_names = db::get_all_blob_names(conn, &hashes)?;

    let metadata = ExportMetadata {
        packages: vec![ExportPackage {
            name: package.to_string(),
            description,
            blobs: export_blobs,
        }],
        blob_names,
    };

    write_export_metadata(to, &metadata)?;
    Ok(())
}

/// Export a list of hashes (no package context) to a target directory.
pub fn export_hashes(conn: &Connection, root: &Path, hashes: &[String], to: &Path) -> Result<()> {
    for hash in hashes {
        copy_blob_to(root, hash, to)?;
    }

    let blob_names = db::get_all_blob_names(conn, hashes)?;

    let metadata = ExportMetadata {
        packages: vec![],
        blob_names,
    };

    write_export_metadata(to, &metadata)?;
    Ok(())
}

fn copy_blob_to(root: &Path, hash: &str, to: &Path) -> Result<()> {
    let src = store::blob_path(root, hash);
    let dest = to.join("sha256").join(&hash[..2]).join(hash);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(&src, &dest).with_context(|| format!("copying blob {} to export dir", hash))?;
    Ok(())
}

fn write_export_metadata(to: &Path, metadata: &ExportMetadata) -> Result<()> {
    let json = serde_json::to_string_pretty(metadata)?;
    fs::write(to.join(EXPORT_FILENAME), json)?;
    Ok(())
}

/// Import blobs and metadata from a directory into the local CAS.
pub fn import_from(conn: &Connection, root: &Path, from: &Path) -> Result<ImportResult> {
    let mut imported_blobs = 0u64;

    let sha_dir = from.join("sha256");
    if sha_dir.is_dir() {
        for prefix_entry in fs::read_dir(&sha_dir)? {
            let prefix_entry = prefix_entry?;
            if !prefix_entry.file_type()?.is_dir() {
                continue;
            }
            for blob_entry in fs::read_dir(prefix_entry.path())? {
                let blob_entry = blob_entry?;
                let hash = blob_entry.file_name().to_string_lossy().to_string();
                let dest = store::blob_path(root, &hash);
                if !dest.exists() {
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::copy(blob_entry.path(), &dest)?;
                    let perms = fs::Permissions::from_mode(0o444);
                    fs::set_permissions(&dest, perms)?;
                }
                db::insert_blob(conn, &hash)?;
                imported_blobs += 1;
            }
        }
    }

    let meta_path = from.join(EXPORT_FILENAME);
    if meta_path.exists() {
        let json = fs::read_to_string(&meta_path)?;
        let metadata: ExportMetadata = serde_json::from_str(&json)?;

        for (hash, names) in &metadata.blob_names {
            for name in names {
                db::insert_blob_name(conn, hash, name)?;
            }
        }

        for pkg in &metadata.packages {
            if !db::package_exists(conn, &pkg.name)? {
                db::create_package(conn, &pkg.name, pkg.description.as_deref())?;
            }
            for blob in &pkg.blobs {
                db::add_blob_to_package(conn, &pkg.name, &blob.hash, blob.path.as_deref())?;
            }
        }
    }

    Ok(ImportResult { imported_blobs })
}

pub struct ImportResult {
    pub imported_blobs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_cas_with_blob(content: &[u8]) -> (TempDir, Connection, String) {
        let root = TempDir::new().unwrap();
        let conn = db::open_db(root.path()).unwrap();
        let src_dir = TempDir::new().unwrap();
        let file = src_dir.path().join("test.bin");
        fs::write(&file, content).unwrap();
        let hash = store::store_blob(root.path(), &file).unwrap();
        db::insert_blob(&conn, &hash).unwrap();
        db::insert_blob_name(&conn, &hash, "test.bin").unwrap();
        drop(src_dir);
        (root, conn, hash)
    }

    #[test]
    fn test_export_package() {
        let (root, conn, hash) = setup_cas_with_blob(b"export me");
        db::create_package(&conn, "testpkg", Some("a test")).unwrap();
        db::add_blob_to_package(&conn, "testpkg", &hash, Some("data/test.bin")).unwrap();

        let export_dir = TempDir::new().unwrap();
        export_package(&conn, root.path(), "testpkg", export_dir.path()).unwrap();

        let exported_blob = export_dir.path().join("sha256").join(&hash[..2]).join(&hash);
        assert!(exported_blob.exists());
        assert_eq!(fs::read(&exported_blob).unwrap(), b"export me");

        let meta_path = export_dir.path().join(EXPORT_FILENAME);
        assert!(meta_path.exists());
        let meta: ExportMetadata = serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
        assert_eq!(meta.packages.len(), 1);
        assert_eq!(meta.packages[0].name, "testpkg");
        assert_eq!(meta.packages[0].blobs.len(), 1);
        assert_eq!(meta.packages[0].blobs[0].hash, hash);
        assert_eq!(meta.blob_names[&hash], vec!["test.bin"]);
    }

    #[test]
    fn test_export_hashes() {
        let (root, conn, hash) = setup_cas_with_blob(b"just a blob");

        let export_dir = TempDir::new().unwrap();
        export_hashes(&conn, root.path(), &[hash.clone()], export_dir.path()).unwrap();

        let exported_blob = export_dir.path().join("sha256").join(&hash[..2]).join(&hash);
        assert!(exported_blob.exists());

        let meta: ExportMetadata = serde_json::from_str(
            &fs::read_to_string(export_dir.path().join(EXPORT_FILENAME)).unwrap()
        ).unwrap();
        assert!(meta.packages.is_empty());
        assert!(meta.blob_names.contains_key(&hash));
    }

    #[test]
    fn test_import_from_export() {
        let (src_root, src_conn, hash) = setup_cas_with_blob(b"roundtrip data");
        db::create_package(&src_conn, "roundtrip", Some("test")).unwrap();
        db::add_blob_to_package(&src_conn, "roundtrip", &hash, Some("file.bin")).unwrap();

        let export_dir = TempDir::new().unwrap();
        export_package(&src_conn, src_root.path(), "roundtrip", export_dir.path()).unwrap();

        let dst_root = TempDir::new().unwrap();
        let dst_conn = db::open_db(dst_root.path()).unwrap();
        let result = import_from(&dst_conn, dst_root.path(), export_dir.path()).unwrap();

        assert_eq!(result.imported_blobs, 1);
        assert!(store::blob_exists(dst_root.path(), &hash));

        let pkgs = db::list_packages(&dst_conn).unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].0, "roundtrip");

        let names = db::get_blob_names(&dst_conn, &hash).unwrap();
        assert!(names.contains(&"test.bin".to_string()));
    }

    #[test]
    fn test_import_merge_semantics() {
        let dst_root = TempDir::new().unwrap();
        let dst_conn = db::open_db(dst_root.path()).unwrap();
        db::insert_blob(&dst_conn, "existing_hash").unwrap();
        db::create_package(&dst_conn, "mypkg", Some("original")).unwrap();
        db::add_blob_to_package(&dst_conn, "mypkg", "existing_hash", None).unwrap();

        let (src_root, src_conn, new_hash) = setup_cas_with_blob(b"new blob");
        db::create_package(&src_conn, "mypkg", Some("from source")).unwrap();
        db::add_blob_to_package(&src_conn, "mypkg", &new_hash, Some("new.bin")).unwrap();

        let export_dir = TempDir::new().unwrap();
        export_package(&src_conn, src_root.path(), "mypkg", export_dir.path()).unwrap();

        let result = import_from(&dst_conn, dst_root.path(), export_dir.path()).unwrap();
        assert_eq!(result.imported_blobs, 1);

        let blobs = db::show_package(&dst_conn, "mypkg").unwrap();
        let hashes: Vec<&str> = blobs.iter().map(|(h, _, _)| h.as_str()).collect();
        assert!(hashes.contains(&"existing_hash"));
        assert!(hashes.contains(&new_hash.as_str()));
    }
}
