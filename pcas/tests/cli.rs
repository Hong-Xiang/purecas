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
    assert!(!path_stdout.contains("[exists]"));
    assert!(!path_stdout.contains("[missing]"));
    assert!(path_stdout.contains(hash));
}

#[test]
fn test_path_missing_blob() {
    let root = cas_root();
    let path_output = pcas()
        .args(["--root", root.path().to_str().unwrap()])
        .args([
            "path",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(path_output.get_output().stdout.clone()).unwrap();
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
