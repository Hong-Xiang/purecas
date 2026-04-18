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

#[test]
fn test_tag_blob() {
    let root = cas_root();
    let src = TempDir::new().unwrap();
    let file = src.path().join("data.bin");
    fs::write(&file, b"tag test").unwrap();

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
            "tag",
            hash,
            "dataset",
            "production",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("dataset"))
        .stdout(predicate::str::contains("production"));
}

#[test]
fn test_meta_blob() {
    let root = cas_root();
    let src = TempDir::new().unwrap();
    let file = src.path().join("data.bin");
    fs::write(&file, b"meta test").unwrap();

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
            "meta",
            hash,
            "trained on ImageNet v2",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("trained on ImageNet v2"));
}

#[test]
fn test_add_with_tag_and_meta() {
    let root = cas_root();
    let src = TempDir::new().unwrap();
    let file = src.path().join("model.pth");
    fs::write(&file, b"model weights").unwrap();

    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "add",
            file.to_str().unwrap(),
            "--tag",
            "model",
            "--tag",
            "v1",
            "--meta",
            "ResNet50 checkpoint",
        ])
        .assert()
        .success();

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

    // Verify tags were set
    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "tag",
            hash,
            "check",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("model"))
        .stdout(predicate::str::contains("v1"));
}

#[test]
fn test_rel() {
    let root = cas_root();
    let src = TempDir::new().unwrap();
    let file1 = src.path().join("v1.bin");
    let file2 = src.path().join("v2.bin");
    fs::write(&file1, b"version 1").unwrap();
    fs::write(&file2, b"version 2").unwrap();

    let out1 = pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "add",
            file1.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let hash1 = String::from_utf8(out1.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();

    let out2 = pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "add",
            file2.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let hash2 = String::from_utf8(out2.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();

    pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "rel",
            &hash2,
            &hash1,
            "derived from",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("->"))
        .stdout(predicate::str::contains("derived from"));
}

#[test]
fn test_lfs_agent_init() {
    let root = cas_root();
    let input = "{\"event\":\"init\",\"operation\":\"upload\",\"remote\":\"origin\",\"concurrent\":true,\"concurrentbatches\":1}\n\
                 {\"event\":\"terminate\"}\n";
    let output = pcas()
        .args(["--root", root.path().to_str().unwrap(), "lfs-agent"])
        .write_stdin(input)
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("\"event\":\"init\""));
}

#[test]
fn test_lfs_agent_upload_roundtrip() {
    let src = TempDir::new().unwrap();
    let file = src.path().join("upload.bin");
    fs::write(&file, b"lfs upload content").unwrap();
    let file_size = fs::metadata(&file).unwrap().len();

    // Compute hash via pcas add into a throwaway root
    let hash_root = cas_root();
    let add_output = pcas()
        .args([
            "--root",
            hash_root.path().to_str().unwrap(),
            "add",
            file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let add_stdout = String::from_utf8(add_output.stdout).unwrap();
    let hash = add_stdout.split_whitespace().next().unwrap().to_string();

    // Fresh CAS root for LFS upload
    let lfs_root = cas_root();
    let input = format!(
        "{{\"event\":\"init\",\"operation\":\"upload\",\"remote\":\"origin\",\"concurrent\":true,\"concurrentbatches\":1}}\n\
         {{\"event\":\"upload\",\"oid\":\"{hash}\",\"size\":{file_size},\"path\":\"{path}\"}}\n\
         {{\"event\":\"terminate\"}}\n",
        hash = hash,
        file_size = file_size,
        path = file.to_str().unwrap().replace('\\', "\\\\"),
    );
    let output = pcas()
        .args(["--root", lfs_root.path().to_str().unwrap(), "lfs-agent"])
        .write_stdin(input)
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("\"event\":\"complete\""),
        "expected complete event in: {stdout}"
    );
    assert!(
        !stdout.contains("\"error\""),
        "unexpected error in: {stdout}"
    );

    // Verify blob exists in the fresh CAS
    pcas()
        .args(["--root", lfs_root.path().to_str().unwrap(), "path", &hash])
        .assert()
        .success()
        .stdout(predicate::str::contains("[exists]"));
}

#[test]
fn test_lfs_agent_download_roundtrip() {
    let root = cas_root();
    let src = TempDir::new().unwrap();
    let file = src.path().join("download.bin");
    fs::write(&file, b"lfs download content").unwrap();
    let file_size = fs::metadata(&file).unwrap().len();

    // Add file to CAS
    let add_output = pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "add",
            file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let add_stdout = String::from_utf8(add_output.stdout).unwrap();
    let hash = add_stdout.split_whitespace().next().unwrap().to_string();

    // Download via LFS agent from same root
    let input = format!(
        "{{\"event\":\"init\",\"operation\":\"download\",\"remote\":\"origin\",\"concurrent\":true,\"concurrentbatches\":1}}\n\
         {{\"event\":\"download\",\"oid\":\"{hash}\",\"size\":{file_size}}}\n\
         {{\"event\":\"terminate\"}}\n",
        hash = hash,
        file_size = file_size,
    );
    let output = pcas()
        .args(["--root", root.path().to_str().unwrap(), "lfs-agent"])
        .write_stdin(input)
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("\"event\":\"complete\""),
        "expected complete event in: {stdout}"
    );
    assert!(
        stdout.contains(&hash),
        "expected hash {hash} in: {stdout}"
    );
    assert!(
        !stdout.contains("\"error\""),
        "unexpected error in: {stdout}"
    );
}
