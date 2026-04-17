use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

/// A blob entry in a package: (hash, logical_path, known_names).
pub type PackageBlobEntry = (String, Option<String>, Vec<String>);

/// Open (or create) the SQLite database at <root>/purecas.db.
pub fn open_db(root: &Path) -> Result<Connection> {
    let db_path = root.join("purecas.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(&db_path)
        .with_context(|| format!("opening database at {}", db_path.display()))?;
    init_tables(&conn)?;
    Ok(conn)
}

fn init_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS blobs (
            hash TEXT PRIMARY KEY
        );
        CREATE TABLE IF NOT EXISTS blob_names (
            hash TEXT NOT NULL REFERENCES blobs(hash),
            name TEXT NOT NULL,
            UNIQUE(hash, name)
        );
        CREATE TABLE IF NOT EXISTS packages (
            name TEXT PRIMARY KEY,
            description TEXT
        );
        CREATE TABLE IF NOT EXISTS package_blobs (
            package_name TEXT NOT NULL REFERENCES packages(name),
            blob_hash TEXT NOT NULL REFERENCES blobs(hash),
            path TEXT,
            UNIQUE(package_name, blob_hash)
        );",
    )?;
    Ok(())
}

/// Insert a blob hash into the DB. Idempotent.
pub fn insert_blob(conn: &Connection, hash: &str) -> Result<()> {
    conn.execute("INSERT OR IGNORE INTO blobs (hash) VALUES (?1)", [hash])?;
    Ok(())
}

/// Record a filename associated with a blob. Idempotent.
pub fn insert_blob_name(conn: &Connection, hash: &str, name: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO blob_names (hash, name) VALUES (?1, ?2)",
        [hash, name],
    )?;
    Ok(())
}

/// Get all known filenames for a blob.
pub fn get_blob_names(conn: &Connection, hash: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT name FROM blob_names WHERE hash = ?1")?;
    let names = stmt
        .query_map([hash], |row| row.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(names)
}

/// Create a package. Errors if it already exists.
pub fn create_package(conn: &Connection, name: &str, description: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO packages (name, description) VALUES (?1, ?2)",
        rusqlite::params![name, description],
    )?;
    Ok(())
}

/// List all packages with their blob counts.
pub fn list_packages(conn: &Connection) -> Result<Vec<(String, usize)>> {
    let mut stmt = conn.prepare(
        "SELECT p.name, COUNT(pb.blob_hash)
         FROM packages p
         LEFT JOIN package_blobs pb ON p.name = pb.package_name
         GROUP BY p.name
         ORDER BY p.name",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Associate a blob with a package. Idempotent.
pub fn add_blob_to_package(
    conn: &Connection,
    package: &str,
    hash: &str,
    path: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO package_blobs (package_name, blob_hash, path) VALUES (?1, ?2, ?3)",
        rusqlite::params![package, hash, path],
    )?;
    Ok(())
}

/// Show all blobs in a package: (hash, path, known_names).
pub fn show_package(conn: &Connection, name: &str) -> Result<Vec<PackageBlobEntry>> {
    let mut stmt = conn.prepare(
        "SELECT pb.blob_hash, pb.path
         FROM package_blobs pb
         WHERE pb.package_name = ?1
         ORDER BY pb.blob_hash",
    )?;
    let rows: Vec<(String, Option<String>)> = stmt
        .query_map([name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut result = Vec::new();
    for (hash, path) in rows {
        let names = get_blob_names(conn, &hash)?;
        result.push((hash, path, names));
    }
    Ok(result)
}

/// Remove a package and its blob associations. Blobs themselves are not deleted.
pub fn remove_package(conn: &Connection, name: &str) -> Result<()> {
    conn.execute("DELETE FROM package_blobs WHERE package_name = ?1", [name])?;
    conn.execute("DELETE FROM packages WHERE name = ?1", [name])?;
    Ok(())
}

/// Check if a name corresponds to a known package.
pub fn package_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM packages WHERE name = ?1",
        [name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Get all blob_names entries as a map: hash -> Vec<name>.
pub fn get_all_blob_names(
    conn: &Connection,
    hashes: &[String],
) -> Result<std::collections::HashMap<String, Vec<String>>> {
    let mut map = std::collections::HashMap::new();
    for hash in hashes {
        let names = get_blob_names(conn, hash)?;
        if !names.is_empty() {
            map.insert(hash.clone(), names);
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_db() -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let conn = open_db(dir.path()).unwrap();
        (dir, conn)
    }

    #[test]
    fn test_open_db_creates_tables() {
        let (_dir, conn) = test_db();
        conn.query_row("SELECT count(*) FROM blobs", [], |_| Ok(()))
            .unwrap();
        conn.query_row("SELECT count(*) FROM blob_names", [], |_| Ok(()))
            .unwrap();
        conn.query_row("SELECT count(*) FROM packages", [], |_| Ok(()))
            .unwrap();
        conn.query_row("SELECT count(*) FROM package_blobs", [], |_| Ok(()))
            .unwrap();
    }

    #[test]
    fn test_insert_blob_and_name() {
        let (_dir, conn) = test_db();
        insert_blob(&conn, "abc123").unwrap();
        insert_blob_name(&conn, "abc123", "file.txt").unwrap();
        let names = get_blob_names(&conn, "abc123").unwrap();
        assert_eq!(names, vec!["file.txt"]);
    }

    #[test]
    fn test_insert_blob_idempotent() {
        let (_dir, conn) = test_db();
        insert_blob(&conn, "abc123").unwrap();
        insert_blob(&conn, "abc123").unwrap();
    }

    #[test]
    fn test_multiple_names_for_same_blob() {
        let (_dir, conn) = test_db();
        insert_blob(&conn, "abc123").unwrap();
        insert_blob_name(&conn, "abc123", "a.txt").unwrap();
        insert_blob_name(&conn, "abc123", "b.txt").unwrap();
        insert_blob_name(&conn, "abc123", "a.txt").unwrap();
        let mut names = get_blob_names(&conn, "abc123").unwrap();
        names.sort();
        assert_eq!(names, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn test_create_and_list_packages() {
        let (_dir, conn) = test_db();
        create_package(&conn, "dataset-a", Some("A test dataset")).unwrap();
        create_package(&conn, "dataset-b", None).unwrap();
        let pkgs = list_packages(&conn).unwrap();
        assert_eq!(pkgs.len(), 2);
        assert!(pkgs
            .iter()
            .any(|(name, count)| name == "dataset-a" && *count == 0));
        assert!(pkgs
            .iter()
            .any(|(name, count)| name == "dataset-b" && *count == 0));
    }

    #[test]
    fn test_add_blob_to_package_and_show() {
        let (_dir, conn) = test_db();
        insert_blob(&conn, "hash1").unwrap();
        insert_blob(&conn, "hash2").unwrap();
        insert_blob_name(&conn, "hash1", "video.mp4").unwrap();
        create_package(&conn, "mypkg", None).unwrap();
        add_blob_to_package(&conn, "mypkg", "hash1", Some("videos/vid.mp4")).unwrap();
        add_blob_to_package(&conn, "mypkg", "hash2", None).unwrap();
        let blobs = show_package(&conn, "mypkg").unwrap();
        assert_eq!(blobs.len(), 2);
        let (_, path, names) = blobs.iter().find(|(h, _, _)| h == "hash1").unwrap();
        assert_eq!(path.as_deref(), Some("videos/vid.mp4"));
        assert_eq!(names, &vec!["video.mp4"]);
    }

    #[test]
    fn test_remove_package() {
        let (_dir, conn) = test_db();
        insert_blob(&conn, "hash1").unwrap();
        create_package(&conn, "mypkg", None).unwrap();
        add_blob_to_package(&conn, "mypkg", "hash1", None).unwrap();
        remove_package(&conn, "mypkg").unwrap();
        let pkgs = list_packages(&conn).unwrap();
        assert!(pkgs.is_empty());
    }

    #[test]
    fn test_add_blob_to_package_idempotent() {
        let (_dir, conn) = test_db();
        insert_blob(&conn, "hash1").unwrap();
        create_package(&conn, "mypkg", None).unwrap();
        add_blob_to_package(&conn, "mypkg", "hash1", Some("a.txt")).unwrap();
        add_blob_to_package(&conn, "mypkg", "hash1", Some("a.txt")).unwrap();
    }
}
