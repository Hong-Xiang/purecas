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
fn test_add_single_file() {
    let root = cas_root();
    let src = TempDir::new().unwrap();
    let file = src.path().join("hello.txt");
    fs::write(&file, b"hello world").unwrap();
    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "add",
            file.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
        ))
        .stdout(predicate::str::contains("hello.txt"));
}

#[test]
fn test_add_multiple_files() {
    let root = cas_root();
    let src = TempDir::new().unwrap();
    fs::write(src.path().join("a.txt"), b"aaa").unwrap();
    fs::write(src.path().join("b.txt"), b"bbb").unwrap();
    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "add",
            src.path().join("a.txt").to_str().unwrap(),
            src.path().join("b.txt").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("a.txt"))
        .stdout(predicate::str::contains("b.txt"));
}

#[test]
fn test_path_exists() {
    let root = cas_root();
    let src = TempDir::new().unwrap();
    let file = src.path().join("hello.txt");
    fs::write(&file, b"hello world").unwrap();
    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "add",
            file.to_str().unwrap(),
        ])
        .assert()
        .success();
    let hash = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
    pcas()
        .args(["--root", root.path().to_str().unwrap(), "path", hash])
        .assert()
        .success()
        .stdout(predicate::str::contains("[exists]"));
}

#[test]
fn test_path_missing() {
    let root = cas_root();
    let hash = "0000000000000000000000000000000000000000000000000000000000000000";
    pcas()
        .args(["--root", root.path().to_str().unwrap(), "path", hash])
        .assert()
        .success()
        .stdout(predicate::str::contains("[missing]"));
}

#[test]
fn test_cat_existing_blob() {
    let root = cas_root();
    let src = TempDir::new().unwrap();
    let file = src.path().join("hello.txt");
    fs::write(&file, b"hello world").unwrap();
    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "add",
            file.to_str().unwrap(),
        ])
        .assert()
        .success();
    let hash = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
    pcas()
        .args(["--root", root.path().to_str().unwrap(), "cat", hash])
        .assert()
        .success()
        .stdout(predicate::eq(b"hello world" as &[u8]));
}

#[test]
fn test_cat_missing_blob() {
    let root = cas_root();
    let hash = "0000000000000000000000000000000000000000000000000000000000000000";
    pcas()
        .args(["--root", root.path().to_str().unwrap(), "cat", hash])
        .assert()
        .failure();
}

#[test]
fn test_pkg_create_and_list() {
    let root = cas_root();
    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "pkg",
            "create",
            "mydata",
            "--description",
            "A test dataset",
        ])
        .assert()
        .success();
    pcas()
        .args(["--root", root.path().to_str().unwrap(), "pkg", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("mydata"))
        .stdout(predicate::str::contains("0"));
}

#[test]
fn test_pkg_add_and_show() {
    let root = cas_root();
    let src = TempDir::new().unwrap();
    let file = src.path().join("data.bin");
    fs::write(&file, b"binary data").unwrap();

    let output = pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "add",
            file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let hash = stdout.split_whitespace().next().unwrap();

    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "pkg",
            "create",
            "mypkg",
        ])
        .assert()
        .success();

    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "pkg",
            "add",
            "mypkg",
            hash,
            "--path",
            "data/data.bin",
        ])
        .assert()
        .success();

    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "pkg",
            "show",
            "mypkg",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(hash))
        .stdout(predicate::str::contains("data/data.bin"));
}

#[test]
fn test_pkg_add_multiple_hashes_with_path_errors() {
    let root = cas_root();
    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "pkg",
            "create",
            "mypkg",
        ])
        .assert()
        .success();
    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "pkg",
            "add",
            "mypkg",
            "hash1",
            "hash2",
            "--path",
            "somepath",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--path"));
}

#[test]
fn test_pkg_rm() {
    let root = cas_root();
    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "pkg",
            "create",
            "mypkg",
        ])
        .assert()
        .success();
    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "pkg",
            "rm",
            "mypkg",
        ])
        .assert()
        .success();
    pcas()
        .args(["--root", root.path().to_str().unwrap(), "pkg", "list"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty().or(predicate::str::contains("mypkg").not()));
}

#[test]
fn test_export_package_cli() {
    let root = cas_root();
    let src = TempDir::new().unwrap();
    let file = src.path().join("data.bin");
    fs::write(&file, b"export test data").unwrap();

    let output = pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "add",
            file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let hash = stdout.split_whitespace().next().unwrap();

    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "pkg",
            "create",
            "testpkg",
        ])
        .assert()
        .success();
    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "pkg",
            "add",
            "testpkg",
            hash,
        ])
        .assert()
        .success();

    let export_dir = TempDir::new().unwrap();
    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "export",
            "testpkg",
            "--to",
            export_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Exported package"));

    let blob_file = export_dir.path().join("sha256").join(&hash[..2]).join(hash);
    assert!(blob_file.exists());

    let meta_file = export_dir.path().join("purecas-export.json");
    assert!(meta_file.exists());
}

#[test]
fn test_export_import_roundtrip() {
    let root1 = cas_root();
    let src = TempDir::new().unwrap();
    let file = src.path().join("roundtrip.txt");
    fs::write(&file, b"roundtrip content").unwrap();

    let output = pcas()
        .args([
            "--root",
            root1.path().to_str().unwrap(),
            "add",
            file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let hash = stdout.split_whitespace().next().unwrap();

    pcas()
        .args([
            "--root",
            root1.path().to_str().unwrap(),
            "pkg",
            "create",
            "rtpkg",
        ])
        .assert()
        .success();
    pcas()
        .args([
            "--root",
            root1.path().to_str().unwrap(),
            "pkg",
            "add",
            "rtpkg",
            hash,
        ])
        .assert()
        .success();

    let export_dir = TempDir::new().unwrap();
    pcas()
        .args([
            "--root",
            root1.path().to_str().unwrap(),
            "export",
            "rtpkg",
            "--to",
            export_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let root2 = cas_root();
    pcas()
        .args([
            "--root",
            root2.path().to_str().unwrap(),
            "import",
            "--from",
            export_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Imported"));

    pcas()
        .args(["--root", root2.path().to_str().unwrap(), "path", hash])
        .assert()
        .success()
        .stdout(predicate::str::contains("[exists]"));

    pcas()
        .args([
            "--root",
            root2.path().to_str().unwrap(),
            "pkg",
            "show",
            "rtpkg",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(hash));
}
