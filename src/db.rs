use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::HashMap;
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
        );
        CREATE TABLE IF NOT EXISTS tags (
            id TEXT NOT NULL,
            tag TEXT NOT NULL,
            UNIQUE(id, tag)
        );
        CREATE TABLE IF NOT EXISTS metadata (
            id TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS blob_relations (
            source_hash TEXT NOT NULL,
            target_hash TEXT NOT NULL,
            note TEXT,
            UNIQUE(source_hash, target_hash)
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

// --- Tags ---

/// Add a tag to a blob or package. Idempotent.
pub fn add_tag(conn: &Connection, id: &str, tag: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO tags (id, tag) VALUES (?1, ?2)",
        [id, tag],
    )?;
    Ok(())
}

/// Get all tags for a blob or package.
pub fn get_tags(conn: &Connection, id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT tag FROM tags WHERE id = ?1 ORDER BY tag")?;
    let tags = stmt
        .query_map([id], |row| row.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(tags)
}

/// Get all tags as a map for a list of IDs.
pub fn get_all_tags(conn: &Connection, ids: &[String]) -> Result<HashMap<String, Vec<String>>> {
    let mut map = HashMap::new();
    for id in ids {
        let tags = get_tags(conn, id)?;
        if !tags.is_empty() {
            map.insert(id.clone(), tags);
        }
    }
    Ok(map)
}

// --- Metadata ---

/// Set metadata string for a blob or package. Overwrites any existing value.
pub fn set_metadata(conn: &Connection, id: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO metadata (id, value) VALUES (?1, ?2)",
        [id, value],
    )?;
    Ok(())
}

/// Get metadata string for a blob or package.
pub fn get_metadata(conn: &Connection, id: &str) -> Result<Option<String>> {
    let result = conn.query_row("SELECT value FROM metadata WHERE id = ?1", [id], |row| {
        row.get(0)
    });
    match result {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Get all metadata as a map for a list of IDs.
pub fn get_all_metadata(conn: &Connection, ids: &[String]) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    for id in ids {
        if let Some(value) = get_metadata(conn, id)? {
            map.insert(id.clone(), value);
        }
    }
    Ok(map)
}

// --- Relations ---

/// Add a relation between two blobs. Idempotent (overwrites note).
pub fn add_relation(
    conn: &Connection,
    source: &str,
    target: &str,
    note: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO blob_relations (source_hash, target_hash, note) VALUES (?1, ?2, ?3)",
        rusqlite::params![source, target, note],
    )?;
    Ok(())
}

/// Get all relations where the given hash is the source.
pub fn get_relations_from(
    conn: &Connection,
    source: &str,
) -> Result<Vec<(String, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT target_hash, note FROM blob_relations WHERE source_hash = ?1 ORDER BY target_hash",
    )?;
    let rows = stmt
        .query_map([source], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Get all relations (for export).
pub fn get_all_relations(
    conn: &Connection,
    hashes: &[String],
) -> Result<Vec<(String, String, Option<String>)>> {
    let mut results = Vec::new();
    for hash in hashes {
        let rels = get_relations_from(conn, hash)?;
        for (target, note) in rels {
            results.push((hash.clone(), target, note));
        }
    }
    Ok(results)
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

    #[test]
    fn test_tags() {
        let (_dir, conn) = test_db();
        insert_blob(&conn, "abc123").unwrap();
        add_tag(&conn, "abc123", "dataset").unwrap();
        add_tag(&conn, "abc123", "production").unwrap();
        add_tag(&conn, "abc123", "dataset").unwrap(); // idempotent
        let tags = get_tags(&conn, "abc123").unwrap();
        assert_eq!(tags, vec!["dataset", "production"]);
    }

    #[test]
    fn test_tags_on_package() {
        let (_dir, conn) = test_db();
        create_package(&conn, "mypkg", None).unwrap();
        add_tag(&conn, "mypkg", "v1").unwrap();
        add_tag(&conn, "mypkg", "stable").unwrap();
        let mut tags = get_tags(&conn, "mypkg").unwrap();
        tags.sort();
        assert_eq!(tags, vec!["stable", "v1"]);
    }

    #[test]
    fn test_metadata() {
        let (_dir, conn) = test_db();
        insert_blob(&conn, "abc123").unwrap();
        assert_eq!(get_metadata(&conn, "abc123").unwrap(), None);
        set_metadata(&conn, "abc123", "trained on ImageNet").unwrap();
        assert_eq!(
            get_metadata(&conn, "abc123").unwrap(),
            Some("trained on ImageNet".to_string())
        );
        // overwrite
        set_metadata(&conn, "abc123", "updated note").unwrap();
        assert_eq!(
            get_metadata(&conn, "abc123").unwrap(),
            Some("updated note".to_string())
        );
    }

    #[test]
    fn test_relations() {
        let (_dir, conn) = test_db();
        insert_blob(&conn, "model_v1").unwrap();
        insert_blob(&conn, "model_v2").unwrap();
        insert_blob(&conn, "dataset_a").unwrap();
        add_relation(&conn, "model_v2", "model_v1", Some("derived from")).unwrap();
        add_relation(&conn, "model_v2", "dataset_a", Some("trained on")).unwrap();
        let rels = get_relations_from(&conn, "model_v2").unwrap();
        assert_eq!(rels.len(), 2);
        assert!(rels.contains(&("dataset_a".to_string(), Some("trained on".to_string()))));
        assert!(rels.contains(&("model_v1".to_string(), Some("derived from".to_string()))));
    }

    #[test]
    fn test_relation_no_note() {
        let (_dir, conn) = test_db();
        insert_blob(&conn, "a").unwrap();
        insert_blob(&conn, "b").unwrap();
        add_relation(&conn, "a", "b", None).unwrap();
        let rels = get_relations_from(&conn, "a").unwrap();
        assert_eq!(rels, vec![("b".to_string(), None)]);
    }
}
