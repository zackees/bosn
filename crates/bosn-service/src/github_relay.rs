//! How a job container's proxy connection reaches the daemon (#650).
//!
//! Job containers run inside the engine's nested Docker; the engine sits on
//! the host's bridge network. Hosts commonly drop container-to-host traffic
//! (this project's development host does), so the daemon cannot simply
//! listen on the bridge. Every connection is therefore opened host to
//! engine, which `docker exec` and the engine's own address always allow:
//!
//! - In the engine, two busybox `nc -lk -e` listeners run per run (no new
//!   binary enters the engine): the **job port**, which job containers use
//!   as `HTTPS_PROXY`, and the **channel port**, bound to the engine's
//!   bridge address, which only the daemon dials.
//! - The daemon keeps a small pool of idle channels open to the channel
//!   port. Each starts with the run's relay nonce; a channel that does not
//!   is closed. An idle channel parks a FIFO pair in the run's relay
//!   directory.
//! - A job connection claims one idle channel (an atomic `mv` of its
//!   marker) and splices its socket to the FIFOs. The daemon sees the
//!   client's first bytes on that channel and serves it with
//!   [`crate::github_connect::serve_client`], then dials a new idle channel.
//!
//! The relay is a dumb byte pipe: TLS, the fake token, the allowlist and the
//! real credential all stay in the daemon.

use std::io::Write;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Idle channels kept open per run: the number of job connections served at
/// once. A job connection that finds none waits up to 30 s for one.
pub const POOL_SIZE: usize = 8;
const RELAY_PORT_BASE: u16 = 42_000;

/// The run's relay ports from its slot: (channel port, job port). Slots are
/// unique within the engine, as for the artifact and cache ports.
#[must_use]
pub fn ports(slot: u16) -> (u16, u16) {
    (RELAY_PORT_BASE + 2 * slot, RELAY_PORT_BASE + 2 * slot + 1)
}

/// The run's relay directory in the engine.
#[must_use]
pub fn dir(work_root: &str, key: &str) -> String {
    format!("{work_root}/ghp/{key}")
}

/// Run on each daemon channel: check the nonce, park a FIFO pair, splice.
const PARK: &str = r#"D="$1"
IFS= read -r hello || exit 0
[ "$hello" = "$(cat "$D/hello")" ] || exit 0
c="$D/ch/$$"
mkfifo "$c.up" "$c.down" || exit 0
trap 'rm -f "$c.up" "$c.down" "$D/idle/$$" "$D/busy/$$"' EXIT
: > "$D/idle/$$"
exec 3<&0
cat "$c.up" &
up=$!
cat <&3 > "$c.down"
wait "$up"
"#;

/// Run on each job connection: claim an idle channel and splice to it.
const CLAIM: &str = r#"D="$1"
n=0
while [ "$n" -lt 600 ]; do
  for m in "$D"/idle/*; do
    [ -e "$m" ] || continue
    id="${m##*/}"
    if mv "$m" "$D/busy/$id" 2>/dev/null; then
      exec 3<&0
      cat <&3 > "$D/ch/$id.up" &
      up=$!
      cat "$D/ch/$id.down"
      kill "$up" 2>/dev/null
      exit 0
    fi
  done
  n=$((n + 1))
  usleep 50000
done
"#;

/// Start the run's relay in the engine and print the engine's bridge
/// address. The nonce arrives on stdin, never in argv.
#[must_use]
pub fn start_script(dir: &str, channel_port: u16, job_port: u16) -> String {
    format!(
        r#"set -eu
D='{dir}'
rm -rf "$D"
mkdir -p "$D/idle" "$D/busy" "$D/ch"
chmod 700 "$D"
IFS= read -r hello
printf '%s\n' "$hello" > "$D/hello"
cat > "$D/park.sh" <<'BOSN_PARK'
{PARK}BOSN_PARK
cat > "$D/claim.sh" <<'BOSN_CLAIM'
{CLAIM}BOSN_CLAIM
addr=$(ip -4 -o addr show eth0 | awk '{{split($4, a, "/"); print a[1]; exit}}')
[ -n "$addr" ]
setsid nc -lk -s "$addr" -p {channel_port} -e sh "$D/park.sh" "$D" </dev/null >/dev/null 2>&1 &
echo $! > "$D/park.pid"
setsid nc -lk -p {job_port} -e sh "$D/claim.sh" "$D" </dev/null >/dev/null 2>&1 &
echo $! > "$D/claim.pid"
printf '%s\n' "$addr"
"#
    )
}

/// Stop the run's relay and remove its directory.
#[must_use]
pub fn stop_script(dir: &str) -> String {
    format!(
        r#"D='{dir}'
for p in "$D/park.pid" "$D/claim.pid"; do [ -f "$p" ] && kill "$(cat "$p")" 2>/dev/null || true; done
pkill -f "$D/" 2>/dev/null || true
rm -rf "$D"
"#
    )
}

/// The daemon's side: [`POOL_SIZE`] workers, each dialling one idle channel
/// at a time and serving the client that claims it. Dropping it closes
/// every channel and stops the workers.
pub struct RelayPool {
    stop: Arc<AtomicBool>,
    open: Arc<Mutex<Vec<TcpStream>>>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl RelayPool {
    pub fn start(
        channel: SocketAddr,
        hello: String,
        serve: Arc<dyn Fn(TcpStream) + Send + Sync>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let open: Arc<Mutex<Vec<TcpStream>>> = Arc::default();
        let workers = (0..POOL_SIZE)
            .map(|_| {
                let (stop, open, hello, serve) =
                    (stop.clone(), open.clone(), hello.clone(), serve.clone());
                std::thread::spawn(move || worker(channel, &hello, &stop, &open, serve.as_ref()))
            })
            .collect();
        Self {
            stop,
            open,
            workers,
        }
    }
}

fn worker(
    channel: SocketAddr,
    hello: &str,
    stop: &AtomicBool,
    open: &Mutex<Vec<TcpStream>>,
    serve: &(dyn Fn(TcpStream) + Send + Sync),
) {
    let mut backoff = Duration::from_millis(50);
    while !stop.load(Ordering::SeqCst) {
        let Ok(mut stream) = TcpStream::connect_timeout(&channel, Duration::from_secs(5)) else {
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(2));
            continue;
        };
        backoff = Duration::from_millis(50);
        if stream.write_all(format!("{hello}\n").as_bytes()).is_err() {
            continue;
        }
        if let (Ok(clone), Ok(mut open)) = (stream.try_clone(), open.lock()) {
            open.push(clone);
        }
        if stop.load(Ordering::SeqCst) {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        let peer = stream.peer_addr().ok();
        let local = stream.local_addr().ok();
        // Park with no timeout until a job connection claims the channel: a
        // parked channel that gave up would leave a stale FIFO pair that hands
        // a later client an empty reply.
        let _ = stream.set_read_timeout(None);
        let mut first = [0u8; 1];
        if matches!(stream.peek(&mut first), Ok(n) if n > 0)
            && let Ok(served) = stream.try_clone()
        {
            serve(served);
        }
        let _ = stream.shutdown(Shutdown::Both);
        if let Ok(mut open) = open.lock() {
            open.retain(|s| s.local_addr().ok() != local || s.peer_addr().ok() != peer);
        }
    }
}

impl Drop for RelayPool {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(open) = self.open.lock() {
            for stream in open.iter() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_are_per_slot_and_clear_of_the_artifact_and_cache_ports() {
        assert_eq!(ports(0), (42_000, 42_001));
        assert_eq!(ports(3), (42_006, 42_007));
        assert!(ports(999).1 > 40_000 + 2 * 999 + 1);
    }

    #[test]
    fn the_nonce_travels_on_stdin_and_scripts_stay_busybox() {
        let script = start_script("/w/ghp/abc", 42_000, 42_001);
        assert!(script.contains("read -r hello"));
        assert!(script.contains("-p 42000 -e sh"));
        assert!(script.contains("-p 42001 -e sh"));
        assert!(
            script.contains("-s \"$addr\""),
            "the channel port binds the bridge address only"
        );
        // POSIX sh backgrounds read /dev/null unless stdin is redirected
        // explicitly, so the splices go through fd 3.
        assert!(PARK.contains("cat <&3"));
        assert!(CLAIM.contains("cat <&3"));
        assert!(stop_script("/w/ghp/abc").contains("rm -rf \"$D\""));
    }
}
