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
fn test_index_then_path_roundtrip() {
    let root = cas_root();
    let file = root.path().join("test.txt");
    fs::write(&file, b"content").unwrap();

    let index_output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["index"])
        .assert()
        .success();
    let index_stdout = String::from_utf8(index_output.get_output().stdout.clone()).unwrap();
    assert!(index_stdout.contains("indexed=1"));
    let hash = index_stdout.split_whitespace().next().unwrap();

    let path_output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["path", hash])
        .assert()
        .success();
    let path_stdout = String::from_utf8(path_output.get_output().stdout.clone()).unwrap();
    let object_path = path_stdout.trim();
    assert!(object_path.contains(hash));
    assert!(std::path::Path::new(object_path).exists());

    let visible_meta = fs::metadata(&file).unwrap();
    let object_meta = fs::metadata(object_path).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(visible_meta.dev(), object_meta.dev());
    assert_eq!(visible_meta.ino(), object_meta.ino());
}

#[test]
fn test_path_missing_digest_fails() {
    let root = cas_root();
    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args([
            "path",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ])
        .assert()
        .failure();
}

#[test]
fn test_path_malformed_digest_fails() {
    let root = cas_root();
    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["path", "not-a-digest"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("hex"));
}

#[test]
fn test_path_ambiguous_digest_fails() {
    let root = cas_root();
    let digest = "a".repeat(64);
    let shard = root.path().join(".pcas").join("sha256").join(&digest[..2]);
    fs::create_dir_all(&shard).unwrap();
    fs::write(shard.join(format!("{digest}--20260722T130016Z")), b"x").unwrap();
    fs::write(shard.join(format!("{digest}--20260722T140000Z")), b"x").unwrap();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["path", &digest])
        .assert()
        .failure()
        .stderr(predicate::str::contains("ambiguous"));
}

#[test]
fn test_index_and_path_never_create_purecas_db() {
    let root = cas_root();
    let file = root.path().join("no-sqlite.txt");
    fs::write(&file, b"no sqlite here").unwrap();

    let index_output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["index"])
        .assert()
        .success();
    let index_stdout = String::from_utf8(index_output.get_output().stdout.clone()).unwrap();
    let hash = index_stdout.split_whitespace().next().unwrap();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["path", hash])
        .assert()
        .success();

    assert!(!root.path().join("purecas.db").exists());
}

#[test]
fn test_index_rejects_legacy_database_before_creating_dot_pcas() {
    let root = cas_root();
    fs::write(root.path().join("visible.bin"), b"visible").unwrap();
    fs::write(root.path().join("purecas.db"), b"legacy sqlite").unwrap();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["index"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("legacy SQLite store")
                .and(predicate::str::contains("separate root")),
        );

    assert!(!root.path().join(".pcas").exists());
    assert_eq!(
        fs::read(root.path().join("visible.bin")).unwrap(),
        b"visible"
    );
}

#[test]
fn test_index_is_idempotent() {
    let root = cas_root();
    fs::write(root.path().join("stable.bin"), b"stable content").unwrap();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["index"])
        .assert()
        .success()
        .stdout(predicate::str::contains("indexed=1"));

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["index"])
        .assert()
        .success()
        .stdout(predicate::str::contains("indexed=0").and(predicate::str::contains("reused=1")));
}

#[test]
fn test_index_deduplicates_identical_content_in_one_run() {
    let root = cas_root();
    fs::write(root.path().join("a.bin"), b"same bytes").unwrap();
    fs::write(root.path().join("b.bin"), b"same bytes").unwrap();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["index"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("indexed=1").and(predicate::str::contains("deduplicated=1")),
        );
}

#[test]
fn test_index_excludes_dot_pcas() {
    let root = cas_root();
    fs::write(root.path().join("visible.txt"), b"visible").unwrap();
    // A pre-existing, well-formed but unreferenced object entry is valid
    // store state; it must never be treated as a discovered visible file.
    let digest = "c".repeat(64);
    let shard = root.path().join(".pcas").join("sha256").join(&digest[..2]);
    fs::create_dir_all(&shard).unwrap();
    fs::write(
        shard.join(format!("{digest}--20260722T130016Z")),
        b"stray object bytes",
    )
    .unwrap();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["index"])
        .assert()
        .success()
        .stdout(predicate::str::contains("indexed=1"));
}

#[test]
fn test_index_basename_pattern() {
    let root = cas_root();
    fs::write(root.path().join("a.mp4"), b"a").unwrap();
    fs::create_dir_all(root.path().join("nested")).unwrap();
    fs::write(root.path().join("nested").join("b.mp4"), b"b").unwrap();
    fs::write(root.path().join("c.txt"), b"c").unwrap();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["index", "*.mp4"])
        .assert()
        .success()
        .stdout(predicate::str::contains("indexed=2"));
}

#[test]
fn test_index_relative_path_pattern() {
    let root = cas_root();
    fs::create_dir_all(root.path().join("data").join("train")).unwrap();
    fs::create_dir_all(root.path().join("data").join("test")).unwrap();
    fs::write(root.path().join("data").join("train").join("a.bin"), b"a").unwrap();
    fs::write(root.path().join("data").join("test").join("b.bin"), b"b").unwrap();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["index", "data/train/*.bin"])
        .assert()
        .success()
        .stdout(predicate::str::contains("indexed=1").and(predicate::str::contains("train")));
}

#[test]
fn test_index_rejects_absolute_pattern() {
    let root = cas_root();
    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["index", "/etc/passwd"])
        .assert()
        .failure();
}

#[test]
fn test_index_rehash_flag_repairs_preserved_mtime_mutation() {
    use filetime::FileTime;

    let root = cas_root();
    let file = root.path().join("mutate.bin");
    fs::write(&file, b"before").unwrap();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["index"])
        .assert()
        .success()
        .stdout(predicate::str::contains("indexed=1"));

    let original_mtime = FileTime::from_last_modification_time(&fs::metadata(&file).unwrap());
    fs::write(&file, b"after-mutation-longer").unwrap();
    filetime::set_file_mtime(&file, original_mtime).unwrap();

    // Without --rehash, the preserved mtime hides the mutation.
    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["index"])
        .assert()
        .success()
        .stdout(predicate::str::contains("reused=1").and(predicate::str::contains("repaired=0")));

    // --rehash always verifies content and repairs the stale object entry.
    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["index", "--rehash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("repaired=1"));

    let expected_hash = sha256_hex(b"after-mutation-longer");
    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["path", &expected_hash])
        .assert()
        .success();
}

#[test]
fn test_index_reports_failures_and_exits_nonzero() {
    use std::os::unix::fs::PermissionsExt;

    let root = cas_root();
    let bad = root.path().join("bad.bin");
    fs::write(&bad, b"unreadable").unwrap();
    fs::write(root.path().join("good.bin"), b"good content").unwrap();

    let mut perms = fs::metadata(&bad).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&bad, perms).unwrap();

    let output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args(["index"])
        .assert()
        .failure();

    // Restore permissions unconditionally so the TempDir cleans up.
    let mut restore = fs::metadata(&bad).unwrap().permissions();
    restore.set_mode(0o644);
    fs::set_permissions(&bad, restore).unwrap();

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("indexed=1"));
    assert!(stdout.contains("failed=1"));
    let stderr = String::from_utf8(output.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("bad.bin"));
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
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

    // `pcas path` now resolves only through the new `.pcas` object index and
    // is unrelated to the legacy import layout; check the legacy blob path
    // directly instead.
    let blob_path = root2.path().join("sha256").join(&hash[..2]).join(hash);
    assert!(blob_path.exists());
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
        .stdout(predicate::str::contains("dataset").and(predicate::str::contains("production")));
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
            "--tag",
            "model",
            "--tag",
            "v1",
            "--meta",
            "ResNet50 pretrained",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let hash = stdout.split_whitespace().next().unwrap();

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
        .stdout(predicate::str::contains("->").and(predicate::str::contains("derived from")));
}

#[test]
fn test_lfs_agent_init() {
    let root = cas_root();
    let init_msg =
        r#"{"event":"init","operation":"upload","concurrent":true,"concurrenttransfers":3}"#;
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
    let expected_hash = "057cab134d8758e5de0f03d63b3ab7e5d5582d89d57a501df241155ac0bfe741";

    let init_msg =
        r#"{"event":"init","operation":"upload","concurrent":true,"concurrenttransfers":1}"#;
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

    let blob_path = root
        .path()
        .join("sha256")
        .join(&expected_hash[..2])
        .join(expected_hash);
    assert!(blob_path.exists());
}

#[test]
fn test_lfs_agent_upload_hash_mismatch() {
    let root = cas_root();
    let upload_file = root.path().join("lfs_bad.bin");
    fs::write(&upload_file, b"lfs content").unwrap();

    let init_msg =
        r#"{"event":"init","operation":"upload","concurrent":true,"concurrenttransfers":1}"#;
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

    let init_msg =
        r#"{"event":"init","operation":"download","concurrent":true,"concurrenttransfers":1}"#;
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
    let init_msg =
        r#"{"event":"init","operation":"download","concurrent":true,"concurrenttransfers":1}"#;
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

#[test]
fn test_serve_binds_and_serves_a_file_over_http_without_touching_purecas_db() {
    use std::io::{BufRead, Read, Write};

    let root = cas_root();
    fs::create_dir_all(root.path().join("sub")).unwrap();
    fs::write(root.path().join("sub/file.txt"), b"served bytes").unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_pcas"))
        .args(["--root", root.path().to_str().unwrap()])
        .args(["serve", "--bind", "127.0.0.1:0"])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawning `pcas serve`");

    // The server prints its bound ephemeral address to stderr before
    // accepting connections; `serve` is dispatched before `Store::open`, so
    // this line appears without ever creating `purecas.db`.
    let stderr = child.stderr.take().unwrap();
    let mut reader = std::io::BufReader::new(stderr);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("reading server startup line");
    assert!(line.contains("listening on http://"), "{line}");
    let addr = line.trim().rsplit("http://").next().unwrap().to_string();

    let mut disabled_post =
        std::net::TcpStream::connect(&addr).expect("connecting to read-only pcas serve");
    write!(
        disabled_post,
        "POST /disabled.bin HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 7\r\nConnection: close\r\n\r\nblocked"
    )
    .unwrap();
    let mut disabled_response = String::new();
    disabled_post
        .read_to_string(&mut disabled_response)
        .unwrap();
    assert!(
        disabled_response.starts_with("HTTP/1.1 405"),
        "{disabled_response}"
    );
    assert!(
        disabled_response.contains("allow: GET, HEAD\r\n"),
        "{disabled_response}"
    );
    assert!(!root.path().join("disabled.bin").exists());

    let mut stream = std::net::TcpStream::connect(&addr).expect("connecting to pcas serve");
    write!(
        stream,
        "GET /sub/file.txt HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("served bytes"), "{response}");
    assert!(
        !root.path().join("purecas.db").exists(),
        "serve must never create purecas.db"
    );

    child.kill().expect("killing server process");
    child.wait().expect("waiting for server process to exit");
}

#[test]
fn test_serve_help_documents_allow_ingest_flag() {
    pcas()
        .args(["serve", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--allow-ingest"));
}

#[test]
fn test_serve_help_documents_process_routes_flag() {
    pcas()
        .args(["serve", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--process-routes"));
}

#[test]
fn test_serve_rejects_invalid_process_config_before_binding() {
    let root = cas_root();
    let config = root.path().join("process-routes.toml");
    let large = fs::File::create(root.path().join("large.bin")).unwrap();
    large.set_len(128 * 1024 * 1024).unwrap();
    fs::write(
        &config,
        r#"
[[process_routes]]
path = "/run"
executable = "/definitely/missing/process-route"
args = []
request_content_type = "application/octet-stream"
response_content_type = "application/octet-stream"
max_request_bytes = 1
max_concurrency = 1
timeout_seconds = 1
"#,
    )
    .unwrap();

    pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args([
            "serve",
            "--bind",
            "127.0.0.1:0",
            "--process-routes",
            config.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("process route executable"));
}

#[test]
fn test_serve_allow_ingest_uploads_and_indexes_without_sqlite() {
    use std::io::{BufRead, Read, Write};

    let root = cas_root();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_pcas"))
        .args(["--root", root.path().to_str().unwrap()])
        .args(["serve", "--bind", "127.0.0.1:0", "--allow-ingest"])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawning writable `pcas serve`");

    let stderr = child.stderr.take().unwrap();
    let mut reader = std::io::BufReader::new(stderr);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("reading server startup line");
    assert!(line.contains("listening on http://"), "{line}");
    let addr = line.trim().rsplit("http://").next().unwrap().to_string();

    let mut upload = std::net::TcpStream::connect(&addr).expect("connecting for upload");
    write!(
        upload,
        "POST /cli/nested.bin HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 12\r\nConnection: close\r\n\r\ncli uploaded"
    )
    .unwrap();
    let mut upload_response = String::new();
    upload.read_to_string(&mut upload_response).unwrap();
    assert!(
        upload_response.starts_with("HTTP/1.1 201"),
        "{upload_response}"
    );
    assert!(
        upload_response.contains("location: /pcas/"),
        "{upload_response}"
    );
    assert!(upload_response.contains("etag: \""), "{upload_response}");

    let mut read_back = std::net::TcpStream::connect(&addr).expect("connecting for read-back");
    write!(
        read_back,
        "GET /cli/nested.bin HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut read_response = String::new();
    read_back.read_to_string(&mut read_response).unwrap();
    assert!(read_response.starts_with("HTTP/1.1 200"), "{read_response}");
    assert!(read_response.ends_with("cli uploaded"), "{read_response}");

    assert_eq!(
        fs::read(root.path().join("cli/nested.bin")).unwrap(),
        b"cli uploaded"
    );
    assert!(root.path().join(".pcas/sha256").exists());
    assert!(!root.path().join("purecas.db").exists());

    child.kill().expect("killing server process");
    child.wait().expect("waiting for server process to exit");
}

#[test]
fn test_second_shutdown_signal_forces_exit_after_group_cleanup() {
    use rustix::process::{kill_process, Pid, Signal};
    use std::io::{BufRead, Write};
    use std::time::{Duration, Instant};

    let root = cas_root();
    let fixture = root.path().join("process-fixture");
    let fixture_source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../purecas/tests/fixtures/process_fixture.rs");
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    assert!(std::process::Command::new(rustc)
        .arg(fixture_source)
        .arg("-O")
        .arg("-o")
        .arg(&fixture)
        .status()
        .unwrap()
        .success());
    let descendant_pid = root.path().join("descendant.pid");
    let config = root.path().join("process-routes.toml");
    fs::write(
        &config,
        format!(
            r#"
[[process_routes]]
path = "/run"
executable = {fixture:?}
args = ["descendant-file", {pid_file:?}]
request_content_type = "application/octet-stream"
response_content_type = "application/octet-stream"
max_request_bytes = 1
max_concurrency = 1
timeout_seconds = 30
"#,
            fixture = fixture.to_string_lossy(),
            pid_file = descendant_pid.to_string_lossy(),
        ),
    )
    .unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_pcas"))
        .args(["--root", root.path().to_str().unwrap()])
        .args([
            "serve",
            "--bind",
            "127.0.0.1:0",
            "--process-routes",
            config.to_str().unwrap(),
        ])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut reader = std::io::BufReader::new(stderr);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let addr = line.trim().rsplit("http://").next().unwrap().to_string();

    let mut process_request = std::net::TcpStream::connect(&addr).unwrap();
    write!(
        process_request,
        "POST /run HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/octet-stream\r\nContent-Length: 0\r\n\r\n"
    )
    .unwrap();
    let started = Instant::now();
    while !descendant_pid.exists() {
        assert!(started.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(10));
    }
    let descendant: u32 = fs::read_to_string(&descendant_pid)
        .unwrap()
        .parse()
        .unwrap();

    let mut stalled = std::net::TcpStream::connect(&addr).unwrap();
    write!(
        stalled,
        "GET /large.bin HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(50));

    let server_pid = Pid::from_raw(child.id() as i32).unwrap();
    kill_process(server_pid, Signal::INT).unwrap();
    kill_process(server_pid, Signal::TERM).unwrap();

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(130));
    assert!(!std::path::Path::new("/proc")
        .join(descendant.to_string())
        .exists());
}
