use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};
use std::path::Path;

use crate::{db, store};

// --- Incoming messages (from git-lfs) ---

#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
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

// --- Agent functions ---

fn send<W: Write>(writer: &mut W, event: &OutgoingEvent) -> Result<()> {
    let line = serde_json::to_string(event)?;
    writeln!(writer, "{}", line)?;
    writer.flush()?;
    Ok(())
}

fn handle_upload<W: Write>(writer: &mut W, root: &Path, oid: &str, path: &str) -> Result<()> {
    let source = Path::new(path);
    let oid_owned = oid.to_string();

    match store::store_blob_with_progress(root, source, |bytes_so_far, bytes_since_last| {
        let _ = send(
            writer,
            &OutgoingEvent::Progress {
                oid: oid_owned.clone(),
                bytes_so_far,
                bytes_since_last,
            },
        );
    }) {
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
                // Best-effort DB registration
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

fn handle_download<W: Write>(writer: &mut W, root: &Path, oid: &str, size: u64) -> Result<()> {
    if !store::blob_exists(root, oid) {
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
    } else {
        let blob = store::blob_path(root, oid);
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
                path: Some(blob.to_string_lossy().into_owned()),
                error: None,
            },
        )?;
    }
    Ok(())
}

/// Testable core: reads from any BufRead, writes to any Write
pub fn run_agent_io<R: BufRead, W: Write>(
    root: &Path,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = input.read_line(&mut line).context("reading input")?;
        if n == 0 {
            break; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let event: IncomingEvent =
            serde_json::from_str(trimmed).context("parsing incoming event")?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    // --- Helpers ---

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

    // --- Parsing tests ---

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
                assert_eq!(remote, Some("origin".to_string()));
                assert_eq!(concurrent, Some(true));
                assert_eq!(concurrent_batches, Some(3));
            }
            _ => panic!("expected Init"),
        }
    }

    #[test]
    fn test_parse_init_download() {
        let json = r#"{"event":"init","operation":"download"}"#;
        let event: IncomingEvent = serde_json::from_str(json).unwrap();
        match event {
            IncomingEvent::Init { operation, .. } => {
                assert_eq!(operation, Operation::Download);
            }
            _ => panic!("expected Init"),
        }
    }

    #[test]
    fn test_parse_upload_event() {
        let json = r#"{"event":"upload","oid":"abc123","size":1024,"path":"/tmp/foo"}"#;
        let event: IncomingEvent = serde_json::from_str(json).unwrap();
        match event {
            IncomingEvent::Upload {
                oid, size, path, ..
            } => {
                assert_eq!(oid, "abc123");
                assert_eq!(size, 1024);
                assert_eq!(path, "/tmp/foo");
            }
            _ => panic!("expected Upload"),
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
            _ => panic!("expected Download"),
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
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["event"], "init");
        assert!(v.get("error").is_none());
    }

    #[test]
    fn test_serialize_init_error() {
        let event = OutgoingEvent::Init {
            error: Some(TransferError {
                code: 1,
                message: "not supported".to_string(),
            }),
        };
        let json = serde_json::to_string(&event).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["event"], "init");
        assert_eq!(v["error"]["code"], 1);
        assert_eq!(v["error"]["message"], "not supported");
    }

    #[test]
    fn test_serialize_progress() {
        let event = OutgoingEvent::Progress {
            oid: "abc".to_string(),
            bytes_so_far: 100,
            bytes_since_last: 50,
        };
        let json = serde_json::to_string(&event).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["event"], "progress");
        assert_eq!(v["oid"], "abc");
        assert_eq!(v["bytesSoFar"], 100);
        assert_eq!(v["bytesSinceLast"], 50);
        // Ensure camelCase, not snake_case
        assert!(v.get("bytes_so_far").is_none());
        assert!(v.get("bytes_since_last").is_none());
    }

    #[test]
    fn test_serialize_complete_upload() {
        let event = OutgoingEvent::Complete {
            oid: "abc".to_string(),
            path: None,
            error: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["event"], "complete");
        assert_eq!(v["oid"], "abc");
        assert!(v.get("path").is_none());
        assert!(v.get("error").is_none());
    }

    #[test]
    fn test_serialize_complete_download() {
        let event = OutgoingEvent::Complete {
            oid: "abc".to_string(),
            path: Some("/cas/sha256/ab/abc".to_string()),
            error: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["event"], "complete");
        assert_eq!(v["path"], "/cas/sha256/ab/abc");
        assert!(v.get("error").is_none());
    }

    #[test]
    fn test_serialize_complete_error() {
        let event = OutgoingEvent::Complete {
            oid: "abc".to_string(),
            path: None,
            error: Some(TransferError {
                code: 2,
                message: "not found".to_string(),
            }),
        };
        let json = serde_json::to_string(&event).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["event"], "complete");
        assert_eq!(v["error"]["code"], 2);
        assert_eq!(v["error"]["message"], "not found");
        assert!(v.get("path").is_none());
    }

    // --- Agent loop tests ---

    #[test]
    fn test_agent_init_and_terminate() {
        let root = TempDir::new().unwrap();
        let output = run_protocol(root.path(), &[
            r#"{"event":"init","operation":"upload"}"#,
            r#"{"event":"terminate"}"#,
        ]);
        let events = parse_output_events(&output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "init");
        assert!(events[0].get("error").is_none());
    }

    #[test]
    fn test_agent_upload() {
        let root = TempDir::new().unwrap();
        let src_dir = root.path().join("staging");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src_file = src_dir.join("hello.bin");
        std::fs::write(&src_file, b"hello world").unwrap();

        // Compute the expected hash
        let expected_hash = store::hash_file(&src_file).unwrap();

        let output = run_protocol(root.path(), &[
            r#"{"event":"init","operation":"upload"}"#,
            &format!(
                r#"{{"event":"upload","oid":"{}","size":11,"path":"{}"}}"#,
                expected_hash,
                src_file.to_string_lossy()
            ),
            r#"{"event":"terminate"}"#,
        ]);
        let events = parse_output_events(&output);

        // First event: init
        assert_eq!(events[0]["event"], "init");

        // Should have at least one progress event and a complete event
        let complete = events.iter().find(|e| e["event"] == "complete").unwrap();
        assert_eq!(complete["oid"], expected_hash);
        assert!(complete.get("error").is_none());

        // Verify blob actually stored in CAS
        assert!(store::blob_exists(root.path(), &expected_hash));
    }

    #[test]
    fn test_agent_upload_hash_mismatch() {
        let root = TempDir::new().unwrap();
        let src_dir = root.path().join("staging");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src_file = src_dir.join("hello.bin");
        std::fs::write(&src_file, b"hello world").unwrap();

        let bad_oid = "0000000000000000000000000000000000000000000000000000000000000000";

        let output = run_protocol(root.path(), &[
            r#"{"event":"init","operation":"upload"}"#,
            &format!(
                r#"{{"event":"upload","oid":"{}","size":11,"path":"{}"}}"#,
                bad_oid,
                src_file.to_string_lossy()
            ),
            r#"{"event":"terminate"}"#,
        ]);
        let events = parse_output_events(&output);
        let complete = events.iter().find(|e| e["event"] == "complete").unwrap();
        let err = complete.get("error").expect("expected error in complete");
        let msg = err["message"].as_str().unwrap();
        assert!(msg.contains("hash mismatch"), "error message was: {}", msg);
    }

    #[test]
    fn test_agent_download() {
        let root = TempDir::new().unwrap();

        // Store a blob first
        let src_dir = root.path().join("staging");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src_file = src_dir.join("hello.bin");
        std::fs::write(&src_file, b"hello world").unwrap();
        let hash = store::store_blob(root.path(), &src_file).unwrap();

        let output = run_protocol(root.path(), &[
            r#"{"event":"init","operation":"download"}"#,
            &format!(
                r#"{{"event":"download","oid":"{}","size":11}}"#,
                hash
            ),
            r#"{"event":"terminate"}"#,
        ]);
        let events = parse_output_events(&output);
        let complete = events.iter().find(|e| e["event"] == "complete").unwrap();
        assert_eq!(complete["oid"], hash);
        assert!(complete.get("error").is_none());
        let path = complete["path"].as_str().unwrap();
        assert!(path.contains(&hash));
    }

    #[test]
    fn test_agent_download_missing() {
        let root = TempDir::new().unwrap();
        let missing_oid = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

        let output = run_protocol(root.path(), &[
            r#"{"event":"init","operation":"download"}"#,
            &format!(
                r#"{{"event":"download","oid":"{}","size":100}}"#,
                missing_oid
            ),
            r#"{"event":"terminate"}"#,
        ]);
        let events = parse_output_events(&output);
        let complete = events.iter().find(|e| e["event"] == "complete").unwrap();
        let err = complete.get("error").expect("expected error");
        let msg = err["message"].as_str().unwrap();
        assert!(msg.contains("not found"), "error message was: {}", msg);
    }

    #[test]
    fn test_agent_upload_progress_events() {
        let root = TempDir::new().unwrap();
        let src_dir = root.path().join("staging");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src_file = src_dir.join("big.bin");

        // 32KB file — store reads in 8KB chunks → expect 4 progress events
        let data = vec![0xABu8; 32768];
        std::fs::write(&src_file, &data).unwrap();
        let expected_hash = store::hash_file(&src_file).unwrap();

        let output = run_protocol(root.path(), &[
            r#"{"event":"init","operation":"upload"}"#,
            &format!(
                r#"{{"event":"upload","oid":"{}","size":32768,"path":"{}"}}"#,
                expected_hash,
                src_file.to_string_lossy()
            ),
            r#"{"event":"terminate"}"#,
        ]);
        let events = parse_output_events(&output);
        let progress_events: Vec<_> = events.iter().filter(|e| e["event"] == "progress").collect();
        assert!(
            progress_events.len() > 1,
            "expected multiple progress events, got {}",
            progress_events.len()
        );
        let last = progress_events.last().unwrap();
        assert_eq!(last["bytesSoFar"], 32768);
    }
}
