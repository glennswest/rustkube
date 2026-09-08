//! Startup preconditions — waiting for what is still being created, rather
//! than exiting and being restarted into working.
//!
//! The control plane starts as a set of processes brought up together: the
//! apiserver, the controller manager and the scheduler are launched at once,
//! and the PKI and tokens the latter two authenticate with are written by
//! whatever bootstraps the node, possibly at the same moment. A client that
//! reads its CA bundle the instant it starts therefore races two things it
//! does not control — the file existing, and the file being *finished* — and
//! loses that race often enough to be the normal case rather than the
//! exception.
//!
//! Losing it used to be fatal: `std::fs::read(ca)?` out of `main` exits the
//! process with status 1, the supervisor restarts it a couple of seconds
//! later, the file is there by then, and the component comes up. Nothing is
//! visibly broken, which is exactly the problem — the retry is load-bearing
//! and nobody knows it. On a slower machine, or a node joining a busy
//! cluster, the same race is a crash loop with no more explanation than it
//! has now.
//!
//! So a missing or half-written credential is treated as "not yet" instead of
//! "no": the process waits for it, says out loud what it is waiting for, and
//! fails only when a bounded timeout says the thing is genuinely not coming.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// How long a component waits for a startup precondition before giving up.
/// Long enough to cover a cold boot writing PKI on a slow disk; short enough
/// that a genuinely absent file is still reported while someone is watching.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the precondition is re-checked.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How often we repeat that we are still waiting.
const REPORT_INTERVAL: Duration = Duration::from_secs(10);

/// Read a PEM file, waiting for it to appear *and be complete*.
///
/// Completeness matters as much as existence: a CA bundle caught mid-write
/// parses as garbage, and `Certificate::from_pem` failing is just as fatal as
/// the open failing. A PEM is considered complete when every `BEGIN` line has
/// a matching `END`.
pub async fn pem_file(path: &Path, what: &str, timeout: Duration) -> io::Result<Vec<u8>> {
    wait_for_file(path, what, timeout, is_complete_pem).await
}

/// [`pem_file`], decoded to a `String` (for APIs that take PEM as text).
pub async fn pem_file_string(path: &Path, what: &str, timeout: Duration) -> io::Result<String> {
    let bytes = pem_file(path, what, timeout).await?;
    String::from_utf8(bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{what}: {e}")))
}

/// Read a token file, waiting for it to appear with something in it, and
/// return the token with surrounding whitespace trimmed.
pub async fn token_file(path: &Path, what: &str, timeout: Duration) -> io::Result<String> {
    let bytes = wait_for_file(path, what, timeout, |b| !trimmed(b).is_empty()).await?;
    Ok(trimmed(&bytes).to_string())
}

/// A PEM is complete when it has at least one block and every `BEGIN` has a
/// closing `END` — the state a file is in only once the writer has finished.
fn is_complete_pem(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let mut begins = 0usize;
    let mut ends = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("-----BEGIN") {
            begins += 1;
        } else if line.starts_with("-----END") && line.ends_with("-----") {
            ends += 1;
        }
    }
    begins > 0 && begins == ends
}

fn trimmed(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap_or("").trim()
}

/// Poll `path` until `ready` accepts its contents, or `timeout` elapses.
///
/// Only the transient conditions wait — absent, empty, half-written. A
/// permission error or a bad path is not going to fix itself and comes back
/// immediately.
async fn wait_for_file(
    path: &Path,
    what: &str,
    timeout: Duration,
    ready: impl Fn(&[u8]) -> bool,
) -> io::Result<Vec<u8>> {
    let start = Instant::now();
    let mut waiting = false;
    let mut last_report = start;

    loop {
        let why = match std::fs::read(path) {
            Ok(bytes) if ready(&bytes) => {
                if waiting {
                    info!(
                        path = %path.display(),
                        waited_secs = start.elapsed().as_secs_f32(),
                        "{what} is ready",
                    );
                }
                return Ok(bytes);
            }
            Ok(bytes) if bytes.is_empty() => "file is empty",
            Ok(_) => "file is incomplete — still being written",
            Err(e) if e.kind() == io::ErrorKind::NotFound => "file does not exist yet",
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("{what} at {}: {e}", path.display()),
                ))
            }
        };

        if start.elapsed() >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "gave up after {}s waiting for {what} at {}: {why}",
                    timeout.as_secs(),
                    path.display(),
                ),
            ));
        }

        if !waiting {
            info!(
                path = %path.display(),
                timeout_secs = timeout.as_secs(),
                "waiting for {what}: {why}",
            );
            waiting = true;
            last_report = Instant::now();
        } else if last_report.elapsed() >= REPORT_INTERVAL {
            warn!(
                path = %path.display(),
                waited_secs = start.elapsed().as_secs(),
                "still waiting for {what}: {why}",
            );
            last_report = Instant::now();
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rk-startup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn half_written_pem_is_not_complete() {
        assert!(!is_complete_pem(b""));
        assert!(!is_complete_pem(b"-----BEGIN CERTIFICATE-----\nMIIB"));
        assert!(is_complete_pem(
            b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n"
        ));
        // A bundle: both blocks must have closed.
        assert!(!is_complete_pem(
            b"-----BEGIN CERTIFICATE-----\nA\n-----END CERTIFICATE-----\n-----BEGIN CERTIFICATE-----\nB\n"
        ));
    }

    #[tokio::test]
    async fn waits_for_a_file_that_arrives_late() {
        let dir = tmpdir();
        let path = dir.join("ca.crt");
        let writer = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            let mut f = std::fs::File::create(&writer).unwrap();
            f.write_all(b"-----BEGIN CERTIFICATE-----\nAA\n-----END CERTIFICATE-----\n")
                .unwrap();
        });
        let bytes = pem_file(&path, "test CA", Duration::from_secs(10))
            .await
            .expect("should have waited for the late file");
        assert!(is_complete_pem(&bytes));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_file_that_never_comes_times_out_with_the_reason() {
        let dir = tmpdir();
        let err = pem_file(
            &dir.join("absent.crt"),
            "test CA",
            Duration::from_millis(300),
        )
        .await
        .expect_err("should not have succeeded");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(err.to_string().contains("does not exist yet"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn token_is_trimmed() {
        let dir = tmpdir();
        let path = dir.join("token");
        std::fs::write(&path, "  abc.def  \n").unwrap();
        let t = token_file(&path, "test token", Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(t, "abc.def");
        std::fs::remove_dir_all(&dir).ok();
    }
}
