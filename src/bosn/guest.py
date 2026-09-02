"""The macOS x86-64 guest stack kind.

A guest stack is a `dockurr/macos` container whose real workload runs in a QEMU/KVM virtual
machine *inside* it. That one fact is what makes it a distinct stack kind rather than a
stack with unusual options, and it decides everything in this module:

- **Create time needs device passthrough.** Without `/dev/kvm` the guest emulates and is
  unusably slow; without `/dev/net/tun` plus `NET_ADMIN` it has no network at all, so no
  sshd, so no way in. These are `docker create` arguments, which is why they are part of the
  generation digest -- Docker cannot re-cap a container after the fact.
- **"Container running" is not "ready".** The container is up in a second; macOS is booted
  minutes later, and on one core, later than that. Readiness is a bounded poll for the
  guest's sshd, with the guest's own logs attached if the deadline passes -- a fixed sleep
  is wrong in both directions, and a wrong-but-invisible one costs a CI job.
- **`docker exec` lands in the wrong namespace.** It runs in the container, beside the VM,
  not inside it. The execution transport is ssh, and a task's real exit code has to come
  back through it or nothing can gate on the result.
- **A bind mount is invisible to the guest.** The manifest refuses `[stack.X.mounts]` on a
  guest stack (see `manifest.StackSpec.__post_init__`); anything the task needs is shipped
  over the same ssh channel that runs it.

Everything here is a pure function of its arguments plus injected probes, so the whole kind
is testable without a host that has KVM -- which no CI runner bosn builds on does.
"""

from __future__ import annotations

import shlex
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

from bosn.engine import EngineError
from bosn.manifest import GuestSpec

# Both guest failures subclass `EngineError` deliberately. They are raised from three
# places -- `converge()` inside a daemon build job, `Converger.run_converged`, and the CLI's
# own foreground execution -- and every one of those already has an `except EngineError`
# that turns a failure into a legible message and an exit code. A bare `RuntimeError` would
# escape all three and surface the single most common real failure ("this host has no
# /dev/kvm") as a traceback.

# `dockur/macos` issue #268: a PCID feature mismatch makes a multi-core guest unstable on
# AMD hosts. Detecting the vendor and forcing one core is better than letting a user
# discover it as a guest that hangs partway through a test run. Intel hosts are unaffected.
AMD_VENDOR = "AuthenticAMD"
AMD_SAFE_CPU_CORES = 1
DEFAULT_CPU_CORES = 2

# Required for the guest to have working virtualization and a network, respectively.
REQUIRED_DEVICES = ("/dev/kvm", "/dev/net/tun")
REQUIRED_CAPABILITIES = ("NET_ADMIN",)

# Tens of gigabytes of guest disk are in flight; a `SIGKILL` after the default 10 seconds
# can leave the filesystem torn. dockur's own documentation asks for a long stop timeout,
# and the working scripts this kind was ported from use 120s.
STOP_TIMEOUT_SECONDS = 120

# ssh's own "could not connect" exit status. It collides with a task that genuinely exits
# 255, which is why the failure text says which one bosn believes it saw rather than
# silently reporting one as the other.
SSH_TRANSPORT_FAILURE = 255

# The guest is a throwaway VM reached over a loopback port that is reused across
# generations, so its host key legitimately changes and there is no trust decision for a
# user to make. Recording it would produce a spurious mismatch on the next rebuild.
SSH_OPTIONS = (
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "LogLevel=ERROR",
)


class GuestUnsupportedError(EngineError):
    """This host cannot run a macOS guest stack, and no retry will change that."""


class GuestNotReadyError(EngineError):
    """The guest's sshd did not answer before the deadline."""


@dataclass(frozen=True)
class HostCapability:
    """What a host offers a guest stack, as one inspectable value."""

    platform: str
    devices_present: tuple[str, ...]
    cpu_vendor: str | None

    @property
    def missing_devices(self) -> tuple[str, ...]:
        return tuple(d for d in REQUIRED_DEVICES if d not in self.devices_present)


def probe_host(
    *,
    platform: str | None = None,
    device_exists: Callable[[str], bool] | None = None,
    cpuinfo: Callable[[], str] | None = None,
) -> HostCapability:
    """Read this host's capability. Every input is injectable so tests need no KVM."""
    import sys

    resolved_platform = sys.platform if platform is None else platform
    exists = device_exists or (lambda path: Path(path).exists())
    return HostCapability(
        platform=resolved_platform,
        devices_present=tuple(device for device in REQUIRED_DEVICES if exists(device)),
        cpu_vendor=_cpu_vendor(cpuinfo or _read_cpuinfo),
    )


def _read_cpuinfo() -> str:
    try:
        return Path("/proc/cpuinfo").read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def _cpu_vendor(read: Callable[[], str]) -> str | None:
    """The host CPU's `vendor_id`, or None when it cannot be read.

    None is not treated as Intel anywhere below: an unknown vendor takes the conservative
    single-core path, because the cost of guessing wrong toward AMD is a slower guest and
    the cost of guessing wrong toward Intel is an unstable one.
    """
    for line in read().splitlines():
        key, separator, value = line.partition(":")
        if separator and key.strip() == "vendor_id":
            return value.strip() or None
    return None


def require_supported_host(stack_name: str, host: HostCapability) -> None:
    """Refuse a guest stack on a host that cannot run it, before touching the engine.

    Everything checked here is a property of the machine, not of anything bosn did, so the
    message names the host condition and stops. Converging first and failing inside
    `docker create` would produce a container that can never work and an error from QEMU
    several layers below the actual cause.
    """
    if not host.platform.startswith("linux"):
        raise GuestUnsupportedError(
            f"stack {stack_name!r} is a macOS guest stack, which needs Linux KVM device "
            f"passthrough; this host reports platform {host.platform!r}. Run it on a Linux "
            "host with /dev/kvm, or select a different stack."
        )
    missing = host.missing_devices
    if missing:
        raise GuestUnsupportedError(
            f"stack {stack_name!r} needs {', '.join(missing)}, which this host does not "
            "expose. /dev/kvm requires hardware virtualization enabled in firmware and a "
            "user with access to it; /dev/net/tun requires the tun module loaded."
        )


def effective_cpu_cores(guest: GuestSpec, host: HostCapability) -> int:
    """How many cores to give the guest.

    An explicit `cpu_cores` in the manifest is honored as written -- if a user has read
    dockur/macos#268 and decided otherwise for their hardware, that is their call. Otherwise
    an AMD host, or a host whose vendor could not be read, gets one core.
    """
    if guest.cpu_cores is not None:
        return guest.cpu_cores
    if host.cpu_vendor == AMD_VENDOR or host.cpu_vendor is None:
        return AMD_SAFE_CPU_CORES
    return DEFAULT_CPU_CORES


def create_args(guest: GuestSpec, host: HostCapability) -> list[str]:
    """The guest-specific half of `docker create`, in a deterministic order.

    Deterministic because these arguments are effectively the container's identity: they are
    digested (see `GuestSpec.digest_fields`), they appear in every log line a human reads
    when a guest misbehaves, and a stable ordering is what lets a test assert on them.
    """
    args: list[str] = []
    for device in REQUIRED_DEVICES:
        args += ["--device", device]
    for capability in REQUIRED_CAPABILITIES:
        args += ["--cap-add", capability]
    args += ["--stop-timeout", str(STOP_TIMEOUT_SECONDS)]
    args += ["--publish", f"{guest.web_port}:8006"]
    args += ["--publish", f"{guest.ssh_port}:22"]
    for key, value in sorted(
        {
            "VERSION": guest.version,
            "RAM_SIZE": guest.ram_size,
            "DISK_SIZE": guest.disk_size,
            "CPU_CORES": str(effective_cpu_cores(guest, host)),
        }.items()
    ):
        args += ["--env", f"{key}={value}"]
    return args


def wait_for_sshd(
    guest: GuestSpec,
    *,
    probe: Callable[[str, int], bool],
    now: Callable[[], float],
    sleep: Callable[[float], None],
    on_wait: Callable[[str], None] | None = None,
) -> float:
    """Poll the guest's sshd until it answers, or raise at the deadline.

    Returns how long the wait took, so a caller can log the real boot time instead of the
    budget. The clock, the sleep, and the probe are all injected: a test must be able to
    exercise both the ready path and the timeout path without waiting minutes for either.
    """
    started = now()
    deadline = started + guest.ready_timeout
    report = on_wait or (lambda _message: None)
    attempts = 0
    while True:
        if probe(guest.ssh_host, guest.ssh_port):
            return now() - started
        attempts += 1
        if now() >= deadline:
            raise GuestNotReadyError(
                f"guest sshd did not answer on {guest.ssh_host}:{guest.ssh_port} within "
                f"{guest.ready_timeout}s ({attempts} attempts). macOS boots slowly, "
                "especially on one core; if this is a first boot the guest may still be "
                "running its installer -- open the dockur web console on "
                f"http://localhost:{guest.web_port} to see where it stopped."
            )
        report(f"waiting for guest sshd on :{guest.ssh_port} ({int(deadline - now())}s left)")
        sleep(guest.ready_poll_interval)


def ensure_ready(
    guest: GuestSpec,
    *,
    fetch_logs: Callable[[], str],
    report: Callable[[str], None] | None = None,
    probe: Callable[[str, int], bool] | None = None,
) -> float:
    """`wait_for_sshd` with the guest's own logs attached to the timeout.

    By the time the deadline passes the caller has already waited out the whole budget, and
    the reason a guest never came up -- a stalled installer, a missing disk, a QEMU refusal
    -- lives only in its logs. Making them go find `docker logs` themselves is the
    fixed-sleep failure mode all over again, so the logs travel with the error.

    Shared by the converge path and the CLI's own foreground execution, which run the task
    through different transports but must fail identically when the guest is not there.
    """
    import time

    try:
        return wait_for_sshd(
            guest,
            probe=probe or tcp_probe,
            now=time.monotonic,
            sleep=time.sleep,
            on_wait=report,
        )
    except GuestNotReadyError as exc:
        raise GuestNotReadyError(
            f"{exc}\n--- guest logs (last 60 lines) ---\n{fetch_logs().strip()}"
        ) from exc


def container_log_reader(engine: object, container: str) -> Callable[[], str]:
    """A callable that fetches the last 60 lines of one container's logs, or explains why not."""

    def read() -> str:
        try:
            result = engine.run(["logs", "--tail", "60", container])  # type: ignore[attr-defined]
        except KeyboardInterrupt:
            raise
        except Exception as exc:  # noqa: BLE001 - a log fetch must never mask the real failure
            return f"(could not read {container} logs: {exc})"
        return getattr(result, "stdout", "") or getattr(result, "stderr", "") or ""

    return read


def tcp_probe(host: str, port: int, *, timeout: float = 3.0) -> bool:
    """True when something accepts a TCP connection at `host:port`.

    A completed handshake is exactly the readiness signal wanted here: the port is published
    by the container from the moment it starts, but nothing listens behind it until the
    guest's own sshd is up, so a connection that completes means macOS has booted far enough
    to serve one.
    """
    import socket

    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except KeyboardInterrupt:
        raise
    except OSError:
        return False


def payload_argv(guest: GuestSpec, source: Path) -> list[str]:
    """`scp` argv that ships this stack's declared payload into the guest before a task.

    This is the answer to "the repo mount cannot be a plain bind": the VM cannot see the
    container's filesystem, so the one artifact a task needs travels over the same ssh
    channel that runs it. One file, not a tree -- the recommended shape is to cross-compile
    on Linux and ship a single prebuilt archive, which is both far smaller than the source
    and removes the guest's whole toolchain burden. See docs/macos-guest.md.
    """
    return scp_argv(guest, source, guest.payload_destination)


def ssh_argv(guest: GuestSpec, command: str) -> list[str]:
    """`ssh` argv that runs one shell command in the guest and returns its exit code."""
    return [
        "ssh",
        *SSH_OPTIONS,
        "-p",
        str(guest.ssh_port),
        f"{guest.ssh_user}@{guest.ssh_host}",
        command,
    ]


def scp_argv(guest: GuestSpec, source: Path | str, destination: str) -> list[str]:
    """`scp` argv that ships one local file to `destination` inside the guest."""
    return [
        "scp",
        *SSH_OPTIONS,
        "-P",
        str(guest.ssh_port),
        str(source),
        f"{guest.ssh_user}@{guest.ssh_host}:{destination}",
    ]


def ship_payload(
    guest: GuestSpec,
    root: Path,
    *,
    timeout: float | None = None,
    runner: Callable[..., object] | None = None,
) -> str | None:
    """Copy the stack's declared payload into the guest. Returns where it landed, or None.

    Runs before every task rather than once at container creation: the payload is a build
    output that changes between runs, and a guest that keeps serving last week's archive
    while reporting success is the worst failure this whole kind can produce.
    """
    if not guest.payload:
        return None
    source = (root / guest.payload).resolve()
    if not source.is_file():
        raise GuestUnsupportedError(
            f"guest payload {guest.payload!r} resolves to {source}, which is not a file. "
            "Build it before running the guest task -- the recommended shape is to "
            "cross-compile on Linux and ship one prebuilt archive (see docs/macos-guest.md)."
        )
    code, output = run_remote(payload_argv(guest, source), timeout=timeout, runner=runner)
    if code != 0:
        raise GuestNotReadyError(
            f"shipping {source.name} into the guest failed ({describe_exit(code)}): {output}"
        )
    return guest.payload_destination


def remote_command(guest: GuestSpec, cmd: str, *, workdir: str | None = None) -> str:
    """The single shell string the guest runs for one task.

    `cmd` is passed to the guest's shell verbatim, exactly as `sh -c` does for a Linux
    stack, so a task's `cmd` means the same thing on both kinds. Only the `cd` is quoted:
    the workdir is a path bosn substitutes, and an unquoted one with a space in it would
    silently truncate.
    """
    if workdir:
        return f"cd {shlex.quote(workdir)} && {cmd}"
    return cmd


def run_remote(
    argv: list[str],
    *,
    timeout: float | None = None,
    runner: Callable[..., object] | None = None,
) -> tuple[int, str]:
    """Run an `ssh`/`scp` argv locally and return `(returncode, combined output)`.

    Deliberately not routed through `engine.Engine`: that class exists to drive the
    container engine binary and prefixes every argv with it. ssh is a peer of docker here,
    not a subcommand of it, and giving Engine a "run something that is not the engine" verb
    would blur the one thing it is for.

    stderr is folded into stdout because a failing remote command's diagnosis is routinely
    split across both, and the caller reports a single block of output either way.
    """
    import subprocess

    run = runner or subprocess.run
    try:
        completed = run(
            argv,
            check=False,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except KeyboardInterrupt:
        raise
    except subprocess.TimeoutExpired as exc:
        raise GuestNotReadyError(
            f"{argv[0]} exceeded its {timeout}-second deadline against the guest"
        ) from exc
    except OSError as exc:
        raise GuestUnsupportedError(
            f"cannot run {argv[0]!r}: {exc}. A macOS guest stack ships and executes its "
            "work over ssh, so an ssh client must be on PATH."
        ) from exc
    stdout = (getattr(completed, "stdout", "") or "").strip()
    stderr = (getattr(completed, "stderr", "") or "").strip()
    combined = "\n".join(part for part in (stdout, stderr) if part)
    return int(getattr(completed, "returncode", 1)), combined


def interactive_remote(
    argv: list[str],
    *,
    timeout: float | None = None,
    runner: Callable[..., object] | None = None,
) -> int:
    """Run an `ssh` argv with this process's stdio inherited.

    Used for `bosn shell` and for an ordinary (non-JSON) `bosn run`, which streams a long
    test run live rather than holding its output to the end. `timeout` is the caller's
    `run_max_duration`, the same deadline the `docker exec` path applies -- inherited stdio
    does not exempt a guest task from it.
    """
    import subprocess

    run = runner or subprocess.run
    try:
        completed = run(argv, check=False, timeout=timeout)
    except KeyboardInterrupt:
        raise
    except subprocess.TimeoutExpired as exc:
        raise GuestNotReadyError(
            f"{argv[0]} exceeded its {timeout}-second deadline against the guest"
        ) from exc
    except OSError as exc:
        raise GuestUnsupportedError(
            f"cannot run {argv[0]!r}: {exc}. A macOS guest stack is reached over ssh, so an "
            "ssh client must be on PATH."
        ) from exc
    return int(getattr(completed, "returncode", 1))


def login_command(workdir: str | None) -> str:
    """The remote command for an interactive `bosn shell` against a guest.

    A login shell, not `sh`: the guest is a full macOS install whose user has a real
    environment, and dropping into a bare `sh` would hide the PATH the task itself runs
    under.
    """
    login = "exec $SHELL -l"
    return f"cd {shlex.quote(workdir)} && {login}" if workdir else login


def describe_exit(returncode: int) -> str:
    """Explain a returncode that ssh may have produced rather than the task."""
    if returncode == SSH_TRANSPORT_FAILURE:
        return (
            f"exit {SSH_TRANSPORT_FAILURE}, which ssh also uses for its own connection "
            "failures; check whether the guest is still reachable before reading this as a "
            "task result"
        )
    return f"exit {returncode}"
