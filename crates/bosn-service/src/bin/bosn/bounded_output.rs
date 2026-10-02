//! Bounded capture of a query child's stdout, shared by `bosn act plan` and
//! the adapter plan (`bosn ci plan --adapter`).

use std::{
    io::Read,
    process::Command,
    sync::mpsc::{self, RecvTimeoutError},
    time::{Duration, Instant},
};

/// Run a short, non-executing query child (an Act listing or a Git
/// observation) to completion. Pipe readers drain concurrently so neither
/// stdout nor stderr can block the child before the deadline; the child is
/// killed and reaped on deadline or when both streams together exceed `limit`.
pub fn bounded_output(
    command: &mut Command,
    deadline: Duration,
    limit: usize,
) -> Result<Vec<u8>, &'static str> {
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|_| "act executable is unavailable")?;
    let (tx, rx) = mpsc::sync_channel::<Option<(bool, Vec<u8>)>>(8);
    for (is_stdout, mut pipe) in [
        (
            true,
            Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
        ),
        (
            false,
            Box::new(child.stderr.take().unwrap()) as Box<dyn Read + Send>,
        ),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut chunk = [0_u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(None);
                        break;
                    }
                    Ok(n) => {
                        if tx.send(Some((is_stdout, chunk[..n].to_vec()))).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    drop(tx);
    let start = Instant::now();
    let mut stdout = Vec::new();
    let mut used = 0_usize;
    let mut eof = 0;
    while eof < 2 {
        let Some(remaining) = deadline.checked_sub(start.elapsed()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("act query timed out");
        };
        match rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(None) => eof += 1,
            Ok(Some((is_stdout, bytes))) => {
                used = used.saturating_add(bytes.len());
                if used > limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("act query output limit exceeded");
                }
                if is_stdout {
                    stdout.extend_from_slice(&bytes)
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("act query output ended unexpectedly");
            }
        }
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("act query timed out");
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("act query could not be reaped");
            }
        }
    };
    if !status.success() {
        return Err("act query failed");
    }
    Ok(stdout)
}
