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
        .stdout(
            predicate::str::contains("indexed=0")
                .and(predicate::str::contains("already_indexed=1")),
        );
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
            predicate::str::contains("indexed=1")
                .and(predicate::str::contains("already_indexed=1")),
        );
}

#[test]
fn test_index_excludes_dot_pcas() {
    let root = cas_root();
    fs::write(root.path().join("visible.txt"), b"visible").unwrap();
    let stray_dir = root.path().join(".pcas").join("sha256").join("aa");
    fs::create_dir_all(&stray_dir).unwrap();
    fs::write(stray_dir.join("stray-file"), b"should not be walked").unwrap();

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
