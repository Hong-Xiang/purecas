use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        Some("eof") => {
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input).unwrap();
            std::io::stdout().write_all(b"eof:").unwrap();
            std::io::stdout()
                .write_all(input.len().to_string().as_bytes())
                .unwrap();
            std::io::stdout().write_all(b":").unwrap();
            std::io::stdout().write_all(&input).unwrap();
        }
        Some("interleave") => {
            let mut buffer = [0_u8; 1024];
            loop {
                let read = std::io::stdin().read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                std::io::stdout().write_all(&buffer[..read]).unwrap();
                std::io::stdout().flush().unwrap();
            }
        }
        Some("slow") => {
            let mut byte = [0_u8; 1];
            while std::io::stdin().read(&mut byte).unwrap() != 0 {
                thread::sleep(Duration::from_millis(1));
                std::io::stdout().write_all(&byte).unwrap();
                std::io::stdout().flush().unwrap();
            }
        }
        Some("early-fail") => {
            eprintln!("early fixture failure");
            std::process::exit(7);
        }
        Some("early-fail-descendant") => {
            let executable = std::env::current_exe().unwrap();
            Command::new(executable).arg("sleep").spawn().unwrap();
            eprintln!("early fixture failure with descendant");
            std::process::exit(7);
        }
        Some("exit-descendant-inherit") => {
            let executable = std::env::current_exe().unwrap();
            Command::new(executable).arg("sleep").spawn().unwrap();
        }
        Some("late-fail") => {
            std::io::stdout().write_all(b"partial").unwrap();
            std::io::stdout().flush().unwrap();
            eprintln!("late fixture failure");
            std::process::exit(9);
        }
        Some("empty") => {
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input).unwrap();
        }
        Some("exit") => {}
        Some("flood-sleep") => {
            std::io::stdout()
                .write_all(&vec![b'f'; 256 * 1024])
                .unwrap();
            std::io::stdout().flush().unwrap();
            thread::sleep(Duration::from_secs(30));
        }
        Some("stderr-flood") => {
            std::io::stderr().write_all(&vec![b'e'; 96 * 1024]).unwrap();
            std::process::exit(3);
        }
        Some("sleep") => {
            thread::sleep(Duration::from_secs(30));
        }
        Some("descendant") => {
            let executable = std::env::current_exe().unwrap();
            let child = Command::new(executable)
                .arg("sleep")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            writeln!(std::io::stdout(), "{}", child.id()).unwrap();
            std::io::stdout().flush().unwrap();
            thread::sleep(Duration::from_secs(30));
        }
        Some("descendant-file") => {
            let pid_file = args.get(1).expect("descendant-file needs a pid path");
            let executable = std::env::current_exe().unwrap();
            let child = Command::new(executable)
                .arg("sleep")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            std::fs::write(pid_file, child.id().to_string()).unwrap();
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input).unwrap();
            thread::sleep(Duration::from_secs(30));
        }
        Some("exit-descendant-file") | Some("fail-descendant-file") => {
            let pid_file = args.get(1).expect("descendant mode needs a pid path");
            let executable = std::env::current_exe().unwrap();
            let child = Command::new(executable)
                .arg("sleep")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            std::fs::write(pid_file, child.id().to_string()).unwrap();
            if args[0] == "fail-descendant-file" {
                std::process::exit(7);
            }
        }
        Some("argv") => {
            for arg in &args[1..] {
                writeln!(std::io::stdout(), "{arg}").unwrap();
            }
        }
        _ => {
            eprintln!("unknown fixture mode");
            std::process::exit(64);
        }
    }
}
