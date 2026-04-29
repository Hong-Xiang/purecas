# Git LFS Custom Transfer Agent Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `pcas lfs-agent` subcommand that implements the Git LFS custom transfer agent protocol over stdin/stdout, allowing Git LFS to use purecas as its blob storage backend.

**Architecture:** A new `src/lfs.rs` module implements the JSON-lines stdio protocol (init → upload/download → terminate). It reuses existing `store.rs` primitives for blob storage and `db.rs` for metadata registration. A new `store_blob_with_progress` function enables chunked file copy with progress callbacks. The agent is wired into the CLI as a top-level `lfs-agent` subcommand.

**Tech Stack:** Rust, serde/serde_json (already deps), clap (already dep), existing store.rs + db.rs modules.

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `src/store.rs` | Modify | Add `store_blob_with_progress` for chunked copy with callbacks |
| `src/lfs.rs` | Create | Protocol types, message parsing, agent loop, upload/download handlers |
| `src/main.rs` | Modify | Add `mod lfs`, `LfsAgent` command variant, wire to `lfs::run_agent` |
| `tests/cli.rs` | Modify | Integration tests piping JSON protocol through `pcas lfs-agent` |
| `README.md` | Modify | Document `lfs-agent` subcommand and Git config setup |

---

### Task 1: Add `store_blob_with_progress` to store.rs

**Files:**
- Modify: `src/store.rs`

This adds a variant of `store_blob` that reports progress via a callback during the file copy. It uses a single-pass approach: read chunks → hash → write to temp → progress callback → finalize.

- [ ] **Step 1: Write the failing test for `store_blob_with_progress`**

Add to the `#[cfg(test)] mod tests` block in `src/store.rs`:

```rust
#[test]
fn test_store_blob_with_progress() {
    let cas_root = TempDir::new().unwrap();
    let src_dir = TempDir::new().unwrap();
    let file = src_dir.path().join("progress.bin");
    let data = vec![0u8; 32768]; // 32KB = 4 chunks of 8KB
    fs::write(&file, &data).unwrap();

    let progress_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = progress_calls.clone();

    let hash = store_blob_with_progress(cas_root.path(), &file, |so_far, since_last| {
        calls.lock().unwrap().push((so_far, since_last));
    })
    .unwrap();

    // Blob should exist and be readable
    assert!(blob_exists(cas_root.path(), &hash));
    let stored = read_blob(cas_root.path(), &hash).unwrap();
    assert_eq!(stored.len(), 32768);

    // Progress should have been reported
    let calls = progress_calls.lock().unwrap();
    assert!(!calls.is_empty());
    // Last call should have bytes_so_far == file size
    assert_eq!(calls.last().unwrap().0, 32768);
}

#[test]
fn test_store_blob_with_progress_existing() {
    let cas_root = TempDir::new().unwrap();
    let src_dir = TempDir::new().unwrap();
    let file = src_dir.path().join("existing.bin");
    fs::write(&file, b"existing content").unwrap();

    // Store once normally
    let hash1 = store_blob(cas_root.path(), &file).unwrap();

    // Store again with progress — should be a no-op but still report full progress
    let progress_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = progress_calls.clone();
    let hash2 = store_blob_with_progress(cas_root.path(), &file, |so_far, since_last| {
        calls.lock().unwrap().push((so_far, since_last));
    })
    .unwrap();

    assert_eq!(hash1, hash2);
    let calls = calls.lock().unwrap();
    // Progress was still reported during hashing
    assert!(!calls.is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test store::tests::test_store_blob_with_progress -- --nocapture`
Expected: FAIL with "cannot find function `store_blob_with_progress`"

- [ ] **Step 3: Implement `store_blob_with_progress`**

Add this function to `src/store.rs` (after `store_blob`):

```rust
/// Store a file in the CAS with progress reporting.
/// Calls `on_progress(bytes_so_far, bytes_since_last)` during the operation.
/// Uses a single-pass approach: read → hash → write → report progress.
pub fn store_blob_with_progress<F>(root: &Path, source: &Path, mut on_progress: F) -> Result<String>
where
    F: FnMut(u64, u64),
{
    let file_size = fs::metadata(source)
        .with_context(|| format!("reading metadata for {}", source.display()))?
        .len();
    let mut source_file =
        fs::File::open(source).with_context(|| format!("opening {}", source.display()))?;

    // Create a temp file in the CAS root for the single-pass write
    let sha256_dir = root.join("sha256");
    fs::create_dir_all(&sha256_dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(&sha256_dir)
        .context("creating temp file in CAS")?;

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    let mut bytes_so_far: u64 = 0;

    loop {
        let n = source_file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        tmp.write_all(&buf[..n])?;
        bytes_so_far += n as u64;
        on_progress(bytes_so_far, n as u64);
    }

    let hash = format!("{:x}", hasher.finalize());
    let dest = blob_path(root, &hash);

    if dest.exists() {
        // Blob already exists; temp file is dropped and cleaned up automatically
        return Ok(hash);
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    // Persist the temp file to the final location
    tmp.persist(&dest)
        .with_context(|| format!("persisting blob {}", hash))?;
    let perms = fs::Permissions::from_mode(0o444);
    fs::set_permissions(&dest, perms)?;

    // If file was empty, ensure at least one progress call
    if file_size == 0 {
        on_progress(0, 0);
    }

    Ok(hash)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test store::tests::test_store_blob_with_progress -- --nocapture`
Expected: Both `test_store_blob_with_progress` and `test_store_blob_with_progress_existing` PASS

- [ ] **Step 5: Run all store tests to ensure no regressions**

Run: `cargo test store::tests`
Expected: All store tests pass

- [ ] **Step 6: Commit**

```bash
git add src/store.rs
git commit -m "feat(store): add store_blob_with_progress for chunked copy with callbacks"
```

---

### Task 2: Create LFS protocol types and message parsing

**Files:**
- Create: `src/lfs.rs`

Define the serde types for the Git LFS custom transfer agent protocol and test that they parse correctly.

- [ ] **Step 1: Create `src/lfs.rs` with protocol types and parsing tests**

Create `src/lfs.rs` with:

```rust
// Git LFS custom transfer agent protocol implementation.
//
// Protocol: JSON-lines over stdin/stdout.
// Git LFS spawns this process and sends init/upload/download/terminate events.
// The agent responds with init/progress/complete events.
//
// Reference: https://github.com/git-lfs/git-lfs/blob/main/docs/custom-transfers.md

use serde::{Deserialize, Serialize};

// --- Incoming messages (from git-lfs) ---

#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
#[serde(rename_all = "lowercase")]
pub enum IncomingEvent {
    Init {
        operation: Operation,
        remote: Option<String>,
        concurrent: Option<bool>,
        #[serde(rename = "concurrentbatches")]
        concurrent_batches: Option<u32>,
    },
    Upload {
        oid: String,
        size: u64,
        path: String,
        #[serde(default)]
        action: Option<serde_json::Value>,
    },
    Download {
        oid: String,
        size: u64,
        #[serde(default)]
        action: Option<serde_json::Value>,
    },
    Terminate,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    Upload,
    Download,
}

// --- Outgoing messages (to git-lfs) ---

#[derive(Debug, Serialize)]
#[serde(tag = "event")]
#[serde(rename_all = "lowercase")]
pub enum OutgoingEvent {
    Init {
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<TransferError>,
    },
    Progress {
        oid: String,
        #[serde(rename = "bytesSoFar")]
        bytes_so_far: u64,
        #[serde(rename = "bytesSinceLast")]
        bytes_since_last: u64,
    },
    Complete {
        oid: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<TransferError>,
    },
}

#[derive(Debug, Serialize)]
pub struct TransferError {
    pub code: i32,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_init_upload() {
        let json = r#"{"event":"init","operation":"upload","remote":"origin","concurrent":true,"concurrentbatches":3}"#;
        let event: IncomingEvent = serde_json::from_str(json).unwrap();
        match event {
            IncomingEvent::Init {
                operation,
                remote,
                concurrent,
                concurrent_batches,
            } => {
                assert_eq!(operation, Operation::Upload);
                assert_eq!(remote.as_deref(), Some("origin"));
                assert_eq!(concurrent, Some(true));
                assert_eq!(concurrent_batches, Some(3));
            }
            _ => panic!("expected Init event"),
        }
    }

    #[test]
    fn test_parse_init_download() {
        let json = r#"{"event":"init","operation":"download","remote":"origin","concurrent":false}"#;
        let event: IncomingEvent = serde_json::from_str(json).unwrap();
        match event {
            IncomingEvent::Init { operation, .. } => {
                assert_eq!(operation, Operation::Download);
            }
            _ => panic!("expected Init event"),
        }
    }

    #[test]
    fn test_parse_upload_event() {
        let json = r#"{"event":"upload","oid":"abc123","size":1024,"path":"/tmp/lfs/abc123"}"#;
        let event: IncomingEvent = serde_json::from_str(json).unwrap();
        match event {
            IncomingEvent::Upload { oid, size, path, .. } => {
                assert_eq!(oid, "abc123");
                assert_eq!(size, 1024);
                assert_eq!(path, "/tmp/lfs/abc123");
            }
            _ => panic!("expected Upload event"),
        }
    }

    #[test]
    fn test_parse_download_event() {
        let json = r#"{"event":"download","oid":"def456","size":2048}"#;
        let event: IncomingEvent = serde_json::from_str(json).unwrap();
        match event {
            IncomingEvent::Download { oid, size, .. } => {
                assert_eq!(oid, "def456");
                assert_eq!(size, 2048);
            }
            _ => panic!("expected Download event"),
        }
    }

    #[test]
    fn test_parse_terminate() {
        let json = r#"{"event":"terminate"}"#;
        let event: IncomingEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, IncomingEvent::Terminate));
    }

    #[test]
    fn test_serialize_init_success() {
        let event = OutgoingEvent::Init { error: None };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(json, r#"{"event":"init"}"#);
    }

    #[test]
    fn test_serialize_init_error() {
        let event = OutgoingEvent::Init {
            error: Some(TransferError {
                code: 1,
                message: "unsupported".to_string(),
            }),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""error""#));
        assert!(json.contains(r#""unsupported""#));
    }

    #[test]
    fn test_serialize_progress() {
        let event = OutgoingEvent::Progress {
            oid: "abc123".to_string(),
            bytes_so_far: 512,
            bytes_since_last: 512,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""bytesSoFar":512"#));
        assert!(json.contains(r#""bytesSinceLast":512"#));
    }

    #[test]
    fn test_serialize_complete_upload() {
        let event = OutgoingEvent::Complete {
            oid: "abc123".to_string(),
            path: None,
            error: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("path"));
        assert!(!json.contains("error"));
    }

    #[test]
    fn test_serialize_complete_download() {
        let event = OutgoingEvent::Complete {
            oid: "abc123".to_string(),
            path: Some("/cas/sha256/ab/abc123".to_string()),
            error: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""path":"/cas/sha256/ab/abc123""#));
    }

    #[test]
    fn test_serialize_complete_error() {
        let event = OutgoingEvent::Complete {
            oid: "abc123".to_string(),
            path: None,
            error: Some(TransferError {
                code: 2,
                message: "blob not found".to_string(),
            }),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""code":2"#));
        assert!(json.contains(r#""blob not found""#));
    }
}
```

- [ ] **Step 2: Add `mod lfs;` to `src/main.rs`**

Add `mod lfs;` after the existing module declarations at the top of `src/main.rs`:

```rust
mod db;
mod fetch;
mod lfs;
mod store;
mod transfer;
```

- [ ] **Step 3: Run tests to verify parsing works**

Run: `cargo test lfs::tests`
Expected: All 11 parsing/serialization tests PASS

- [ ] **Step 4: Commit**

```bash
git add src/lfs.rs src/main.rs
git commit -m "feat(lfs): add protocol types and message parsing for LFS transfer agent"
```

---

### Task 3: Implement the agent loop

**Files:**
- Modify: `src/lfs.rs`

Add the core agent logic: reading events from stdin, handling upload/download, writing responses to stdout.

- [ ] **Step 1: Add agent loop with unit test**

Add these imports to the top of `src/lfs.rs`:

```rust
use anyhow::{Context, Result};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::{db, store};
```

Add the `send`, `handle_upload`, `handle_download`, and `run_agent` functions after the type definitions (before `#[cfg(test)]`):

```rust
fn send<W: Write>(writer: &mut W, event: &OutgoingEvent) -> Result<()> {
    let line = serde_json::to_string(event)?;
    writeln!(writer, "{}", line)?;
    writer.flush()?;
    Ok(())
}

fn handle_upload<W: Write>(
    writer: &mut W,
    root: &Path,
    oid: &str,
    path: &str,
) -> Result<()> {
    let source = PathBuf::from(path);
    let oid_owned = oid.to_string();

    let result = store::store_blob_with_progress(root, &source, |bytes_so_far, bytes_since_last| {
        let _ = send(
            writer,
            &OutgoingEvent::Progress {
                oid: oid_owned.clone(),
                bytes_so_far,
                bytes_since_last,
            },
        );
    });

    match result {
        Ok(hash) => {
            if hash != oid {
                send(
                    writer,
                    &OutgoingEvent::Complete {
                        oid: oid.to_string(),
                        path: None,
                        error: Some(TransferError {
                            code: 2,
                            message: format!(
                                "hash mismatch: expected {}, got {}",
                                oid, hash
                            ),
                        }),
                    },
                )?;
            } else {
                // Register in DB (best-effort — don't fail the transfer if DB is unavailable)
                if let Ok(conn) = db::open_db(root) {
                    let _ = db::insert_blob(&conn, &hash);
                }
                send(
                    writer,
                    &OutgoingEvent::Complete {
                        oid: oid.to_string(),
                        path: None,
                        error: None,
                    },
                )?;
            }
        }
        Err(e) => {
            send(
                writer,
                &OutgoingEvent::Complete {
                    oid: oid.to_string(),
                    path: None,
                    error: Some(TransferError {
                        code: 3,
                        message: format!("upload failed: {}", e),
                    }),
                },
            )?;
        }
    }
    Ok(())
}

fn handle_download<W: Write>(
    writer: &mut W,
    root: &Path,
    oid: &str,
    size: u64,
) -> Result<()> {
    let blob = store::blob_path(root, oid);
    if !blob.exists() {
        send(
            writer,
            &OutgoingEvent::Complete {
                oid: oid.to_string(),
                path: None,
                error: Some(TransferError {
                    code: 2,
                    message: format!("blob not found: {}", oid),
                }),
            },
        )?;
        return Ok(());
    }

    send(
        writer,
        &OutgoingEvent::Progress {
            oid: oid.to_string(),
            bytes_so_far: size,
            bytes_since_last: size,
        },
    )?;

    send(
        writer,
        &OutgoingEvent::Complete {
            oid: oid.to_string(),
            path: Some(blob.to_string_lossy().to_string()),
            error: None,
        },
    )?;
    Ok(())
}

/// Run the LFS custom transfer agent, reading from `input` and writing to `output`.
/// This is the testable core; `run_agent` wraps it with stdin/stdout.
pub fn run_agent_io<R: BufRead, W: Write>(
    root: &Path,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = input
            .read_line(&mut line)
            .context("reading from input")?;
        if n == 0 {
            break; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let event: IncomingEvent =
            serde_json::from_str(trimmed).with_context(|| format!("parsing event: {}", trimmed))?;

        match event {
            IncomingEvent::Init { .. } => {
                send(output, &OutgoingEvent::Init { error: None })?;
            }
            IncomingEvent::Upload {
                oid, path, ..
            } => {
                handle_upload(output, root, &oid, &path)?;
            }
            IncomingEvent::Download { oid, size, .. } => {
                handle_download(output, root, &oid, size)?;
            }
            IncomingEvent::Terminate => {
                break;
            }
        }
    }
    Ok(())
}

/// Entry point called from main.rs — uses real stdin/stdout.
pub fn run_agent(root: &Path) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    run_agent_io(root, &mut input, &mut output)
}
```

- [ ] **Step 2: Add unit tests for the agent loop**

Add these tests to the existing `#[cfg(test)] mod tests` block in `src/lfs.rs`:

```rust
use std::io::Cursor;
use tempfile::TempDir;
use std::fs;

fn run_protocol(root: &Path, input_lines: &[&str]) -> String {
    let input_str = input_lines.join("\n") + "\n";
    let mut input = Cursor::new(input_str.into_bytes());
    let mut output = Vec::new();
    run_agent_io(root, &mut input, &mut output).unwrap();
    String::from_utf8(output).unwrap()
}

fn parse_output_events(output: &str) -> Vec<serde_json::Value> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn test_agent_init_and_terminate() {
    let root = TempDir::new().unwrap();
    let output = run_protocol(
        root.path(),
        &[
            r#"{"event":"init","operation":"upload","remote":"origin","concurrent":false}"#,
            r#"{"event":"terminate"}"#,
        ],
    );
    let events = parse_output_events(&output);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event"], "init");
    assert!(events[0].get("error").is_none());
}

#[test]
fn test_agent_upload() {
    let cas_root = TempDir::new().unwrap();
    let src_dir = TempDir::new().unwrap();
    let file = src_dir.path().join("test.bin");
    fs::write(&file, b"hello world").unwrap();
    let expected_oid = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

    let output = run_protocol(
        cas_root.path(),
        &[
            r#"{"event":"init","operation":"upload","remote":"origin","concurrent":false}"#,
            &format!(
                r#"{{"event":"upload","oid":"{}","size":11,"path":"{}"}}"#,
                expected_oid,
                file.to_str().unwrap()
            ),
            r#"{"event":"terminate"}"#,
        ],
    );

    let events = parse_output_events(&output);
    // init + at least one progress + complete
    assert!(events.len() >= 3, "expected >=3 events, got {}: {:?}", events.len(), events);
    assert_eq!(events[0]["event"], "init");
    let complete = events.last().unwrap();
    assert_eq!(complete["event"], "complete");
    assert_eq!(complete["oid"], expected_oid);
    assert!(complete.get("error").is_none());

    // Blob should be stored in CAS
    assert!(store::blob_exists(cas_root.path(), expected_oid));
}

#[test]
fn test_agent_upload_hash_mismatch() {
    let cas_root = TempDir::new().unwrap();
    let src_dir = TempDir::new().unwrap();
    let file = src_dir.path().join("test.bin");
    fs::write(&file, b"hello world").unwrap();
    let wrong_oid = "0000000000000000000000000000000000000000000000000000000000000000";

    let output = run_protocol(
        cas_root.path(),
        &[
            r#"{"event":"init","operation":"upload","remote":"origin","concurrent":false}"#,
            &format!(
                r#"{{"event":"upload","oid":"{}","size":11,"path":"{}"}}"#,
                wrong_oid,
                file.to_str().unwrap()
            ),
            r#"{"event":"terminate"}"#,
        ],
    );

    let events = parse_output_events(&output);
    let complete = events.iter().find(|e| e["event"] == "complete").unwrap();
    assert!(complete["error"].is_object());
    assert!(complete["error"]["message"]
        .as_str()
        .unwrap()
        .contains("hash mismatch"));
}

#[test]
fn test_agent_download() {
    let cas_root = TempDir::new().unwrap();
    let src_dir = TempDir::new().unwrap();
    let file = src_dir.path().join("test.bin");
    fs::write(&file, b"hello world").unwrap();
    let oid = store::store_blob(cas_root.path(), &file).unwrap();

    let output = run_protocol(
        cas_root.path(),
        &[
            r#"{"event":"init","operation":"download","remote":"origin","concurrent":false}"#,
            &format!(r#"{{"event":"download","oid":"{}","size":11}}"#, oid),
            r#"{"event":"terminate"}"#,
        ],
    );

    let events = parse_output_events(&output);
    let complete = events.iter().find(|e| e["event"] == "complete").unwrap();
    assert!(complete.get("error").is_none());
    assert!(complete["path"].as_str().unwrap().contains(&oid));
}

#[test]
fn test_agent_download_missing() {
    let cas_root = TempDir::new().unwrap();
    let missing_oid = "0000000000000000000000000000000000000000000000000000000000000000";

    let output = run_protocol(
        cas_root.path(),
        &[
            r#"{"event":"init","operation":"download","remote":"origin","concurrent":false}"#,
            &format!(r#"{{"event":"download","oid":"{}","size":100}}"#, missing_oid),
            r#"{"event":"terminate"}"#,
        ],
    );

    let events = parse_output_events(&output);
    let complete = events.iter().find(|e| e["event"] == "complete").unwrap();
    assert!(complete["error"].is_object());
    assert!(complete["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not found"));
}

#[test]
fn test_agent_upload_progress_events() {
    let cas_root = TempDir::new().unwrap();
    let src_dir = TempDir::new().unwrap();
    let file = src_dir.path().join("big.bin");
    let data = vec![0xABu8; 32768]; // 32KB = multiple chunks
    fs::write(&file, &data).unwrap();
    let oid = store::hash_file(&file).unwrap();

    let output = run_protocol(
        cas_root.path(),
        &[
            r#"{"event":"init","operation":"upload","remote":"origin","concurrent":false}"#,
            &format!(
                r#"{{"event":"upload","oid":"{}","size":{},"path":"{}"}}"#,
                oid,
                data.len(),
                file.to_str().unwrap()
            ),
            r#"{"event":"terminate"}"#,
        ],
    );

    let events = parse_output_events(&output);
    let progress_events: Vec<_> = events.iter().filter(|e| e["event"] == "progress").collect();
    // Multiple progress events for a multi-chunk file
    assert!(
        progress_events.len() > 1,
        "expected multiple progress events, got {}",
        progress_events.len()
    );
    // Last progress event should show all bytes transferred
    let last_progress = progress_events.last().unwrap();
    assert_eq!(last_progress["bytesSoFar"], 32768);
}
```

- [ ] **Step 3: Run agent loop tests**

Run: `cargo test lfs::tests`
Expected: All tests PASS (11 parsing tests + 6 agent tests = 17 total)

- [ ] **Step 4: Commit**

```bash
git add src/lfs.rs
git commit -m "feat(lfs): implement agent loop with upload, download, and progress reporting"
```

---

### Task 4: Wire up the `lfs-agent` subcommand in main.rs

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Add `LfsAgent` variant to the `Commands` enum**

Add this variant inside the `Commands` enum, after the `Rel` variant:

```rust
    /// Run as a Git LFS custom transfer agent (stdin/stdout protocol)
    LfsAgent,
```

- [ ] **Step 2: Add the match arm in `main()`**

Add this arm inside the `match cli.command { ... }` block, after the `Commands::Rel` arm:

```rust
        Commands::LfsAgent => lfs::run_agent(&root),
```

- [ ] **Step 3: Verify the subcommand works**

Run: `cargo build && echo '{"event":"init","operation":"upload","remote":"origin","concurrent":false}' | cargo run -- --root /tmp/pcas-test lfs-agent`
Expected: Outputs a JSON line `{"event":"init"}` and hangs waiting for more input (Ctrl+C to stop). This confirms the subcommand is wired up correctly.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat(cli): wire up lfs-agent subcommand"
```

---

### Task 5: Integration tests

**Files:**
- Modify: `tests/cli.rs`

- [ ] **Step 1: Add integration tests for `lfs-agent`**

Add these tests to the end of `tests/cli.rs`:

```rust
#[test]
fn test_lfs_agent_init() {
    let root = cas_root();
    let input = r#"{"event":"init","operation":"upload","remote":"origin","concurrent":false}
{"event":"terminate"}
"#;
    pcas()
        .args(["--root", root.path().to_str().unwrap(), "lfs-agent"])
        .write_stdin(input)
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""event":"init""#));
}

#[test]
fn test_lfs_agent_upload_roundtrip() {
    let root = cas_root();
    let src = TempDir::new().unwrap();
    let file = src.path().join("lfs-upload.txt");
    fs::write(&file, b"hello lfs").unwrap();
    let expected_hash = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    // Compute actual hash for "hello lfs"
    // Actually, let's add the file first to get the real hash
    let output = pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "add",
            file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout_str = String::from_utf8(output.stdout).unwrap();
    let hash = stdout_str.split_whitespace().next().unwrap().to_string();

    // Now use a fresh CAS root for the LFS agent test
    let lfs_root = cas_root();
    let input = format!(
        r#"{{"event":"init","operation":"upload","remote":"origin","concurrent":false}}
{{"event":"upload","oid":"{}","size":9,"path":"{}"}}
{{"event":"terminate"}}
"#,
        hash,
        file.to_str().unwrap()
    );

    let lfs_output = pcas()
        .args(["--root", lfs_root.path().to_str().unwrap(), "lfs-agent"])
        .write_stdin(input.as_str())
        .output()
        .unwrap();

    let stdout = String::from_utf8(lfs_output.stdout).unwrap();
    assert!(stdout.contains(r#""event":"init""#));
    assert!(stdout.contains(r#""event":"complete""#));
    assert!(!stdout.contains(r#""error""#));

    // Verify blob is in the LFS CAS
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
    let file = src.path().join("lfs-download.txt");
    fs::write(&file, b"download me").unwrap();

    // Add file to CAS first
    let output = pcas()
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "add",
            file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let hash = String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();

    let input = format!(
        r#"{{"event":"init","operation":"download","remote":"origin","concurrent":false}}
{{"event":"download","oid":"{}","size":11}}
{{"event":"terminate"}}
"#,
        hash
    );

    let lfs_output = pcas()
        .args(["--root", root.path().to_str().unwrap(), "lfs-agent"])
        .write_stdin(input.as_str())
        .output()
        .unwrap();

    let stdout = String::from_utf8(lfs_output.stdout).unwrap();
    assert!(stdout.contains(r#""event":"complete""#));
    assert!(stdout.contains(&hash));
    assert!(!stdout.contains(r#""error""#));
}
```

- [ ] **Step 2: Run integration tests**

Run: `cargo test --test cli test_lfs_agent`
Expected: All 3 LFS integration tests PASS

- [ ] **Step 3: Run full test suite**

Run: `cargo test`
Expected: All tests pass (previous 40 + new ~20 LFS tests)

- [ ] **Step 4: Commit**

```bash
git add tests/cli.rs
git commit -m "test: add integration tests for lfs-agent subcommand"
```

---

### Task 6: Update README

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Check README structure**

Read `README.md` to find the right section to add LFS documentation (likely after the existing command documentation).

- [ ] **Step 2: Add LFS agent documentation to README.md**

Add a new section after the existing command documentation. The exact location depends on the README structure, but it should include:

```markdown
## Git LFS Integration

`pcas` can act as a [Git LFS custom transfer agent](https://github.com/git-lfs/git-lfs/blob/main/docs/custom-transfers.md), allowing Git LFS to store large files in your purecas CAS.

### Setup

Add to your `.gitconfig` or repo's `.git/config`:

```gitconfig
[lfs "customtransfer.pcas"]
    path = pcas
    args = "lfs-agent"
[lfs]
    standalonetransferagent = pcas
```

If your CAS root isn't the default (`~/data/blob`), pass it via args:

```gitconfig
[lfs "customtransfer.pcas"]
    path = pcas
    args = "--root /path/to/cas lfs-agent"
```

Or set the `CAS_ROOT` environment variable.

### How It Works

When you `git push` or `git pull`, Git LFS spawns `pcas lfs-agent` and communicates via a JSON protocol over stdin/stdout. The agent:

- **Upload (`git push`):** Stores the blob in the CAS (`sha256/<prefix>/<hash>`), verifies the hash matches the LFS OID, and registers it in the metadata DB.
- **Download (`git pull`):** Returns the CAS path for the blob, which Git LFS reads directly.

Progress is reported during transfers, so `git push`/`git pull` show transfer progress as usual.

### Manual Testing

```bash
echo '{"event":"init","operation":"upload","remote":"origin","concurrent":false}' | pcas lfs-agent
# → {"event":"init"}
```
```

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: add Git LFS integration section to README"
```
