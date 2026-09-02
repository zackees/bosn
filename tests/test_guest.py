"""The macOS x86-64 guest stack kind (#151).

Every probe, clock, and subprocess is injected, so this whole file runs on a host with no
KVM, no `dockurr/macos` image, and no ssh client -- which is every CI runner bosn builds on.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from bosn import guest
from bosn.guest import GuestNotReadyError, GuestUnsupportedError, HostCapability
from bosn.manifest import GUEST_MACOS_X64, GuestSpec, ManifestError, parse

INTEL_HOST = HostCapability(
    platform="linux", devices_present=guest.REQUIRED_DEVICES, cpu_vendor="GenuineIntel"
)
AMD_HOST = HostCapability(
    platform="linux", devices_present=guest.REQUIRED_DEVICES, cpu_vendor=guest.AMD_VENDOR
)


def guest_manifest(root: Path, **stack_overrides: object):
    body: dict[str, object] = {
        "kind": GUEST_MACOS_X64,
        "acknowledge_macos_license": True,
        "image": "ghcr.io/example/macos-x64-guest:ventura",
        "family": "macos-x64",
    }
    body.update(stack_overrides)
    return parse({"stack": {"mac": body}}, root)


# -- manifest surface -------------------------------------------------------


def test_a_guest_stack_needs_an_explicit_licence_acknowledgement(tmp_path: Path) -> None:
    with pytest.raises(ManifestError) as exc:
        parse({"stack": {"mac": {"kind": GUEST_MACOS_X64, "image": "x"}}}, tmp_path)
    assert "acknowledge_macos_license" in str(exc.value)


def test_the_acknowledgement_is_meaningless_without_the_guest_kind(tmp_path: Path) -> None:
    # Otherwise a copied stanza would leave a licence claim attached to a Linux stack.
    with pytest.raises(ManifestError):
        parse({"stack": {"s": {"image": "x", "acknowledge_macos_license": True}}}, tmp_path)


def test_a_guest_stack_refuses_bind_mounts(tmp_path: Path) -> None:
    """A bind lands in the container; the VM inside it cannot see one."""
    with pytest.raises(ManifestError) as exc:
        guest_manifest(
            tmp_path,
            mounts={"repo": {"source": ".", "destination": "/repo", "readonly": True}},
        )
    assert "cannot see host bind mounts" in str(exc.value)


def test_a_guest_stack_defaults_its_guest_table(tmp_path: Path) -> None:
    stack = guest_manifest(tmp_path).stack("mac")
    assert stack.is_guest
    assert stack.guest_spec() == GuestSpec()


def test_the_guest_table_refuses_unknown_keys(tmp_path: Path) -> None:
    # A silently dropped `ssh_port` would fail as a connection error a long way from here.
    with pytest.raises(ManifestError) as exc:
        guest_manifest(tmp_path, guest={"ssh_prot": 2222})
    assert "ssh_prot" in str(exc.value)


def test_the_guest_table_refuses_a_colliding_port_pair(tmp_path: Path) -> None:
    with pytest.raises(ManifestError) as exc:
        guest_manifest(tmp_path, guest={"ssh_port": 8006})
    assert "must differ" in str(exc.value)


def test_an_unknown_stack_kind_is_refused(tmp_path: Path) -> None:
    with pytest.raises(ManifestError) as exc:
        parse({"stack": {"s": {"image": "x", "kind": "macos-arm64"}}}, tmp_path)
    assert "unknown kind" in str(exc.value)


def test_a_guest_table_without_the_guest_kind_is_refused(tmp_path: Path) -> None:
    with pytest.raises(ManifestError):
        parse({"stack": {"s": {"image": "x", "guest": {"ssh_port": 2222}}}}, tmp_path)


def test_guest_configuration_is_part_of_the_generation_digest(tmp_path: Path) -> None:
    """Ports and sizing land in `docker create`, which Docker cannot revise afterwards."""
    baseline = guest_manifest(tmp_path).digest("mac")
    moved = guest_manifest(tmp_path, guest={"ssh_port": 2299}).digest("mac")
    assert baseline != moved


def test_a_plain_stack_digest_is_unchanged_by_the_guest_feature(tmp_path: Path) -> None:
    """Pinned to the value main produced, so upgrading never rolls a live generation."""
    manifest = parse(
        {"stack": {"t": {"image": "x", "volumes": {"v": {"scope": "machine"}}}}}, tmp_path
    )
    assert (
        manifest.digest("t")
        == "sha256:45a1096c85b98ebbf505795fa547bc8c6be8706e42160eece61df5b19e904291"
    )


# -- host preflight ---------------------------------------------------------


def test_a_non_linux_host_is_refused_by_name() -> None:
    host = HostCapability(platform="darwin", devices_present=(), cpu_vendor=None)
    with pytest.raises(GuestUnsupportedError) as exc:
        guest.require_supported_host("mac", host)
    assert "darwin" in str(exc.value)


def test_a_linux_host_without_kvm_is_refused_and_says_which_device() -> None:
    host = HostCapability(platform="linux", devices_present=("/dev/net/tun",), cpu_vendor=None)
    with pytest.raises(GuestUnsupportedError) as exc:
        guest.require_supported_host("mac", host)
    assert "/dev/kvm" in str(exc.value)


def test_a_capable_linux_host_is_accepted() -> None:
    guest.require_supported_host("mac", INTEL_HOST)


def test_probe_host_reads_the_cpu_vendor_and_devices() -> None:
    host = guest.probe_host(
        platform="linux",
        device_exists=lambda path: path == "/dev/kvm",
        cpuinfo=lambda: "processor\t: 0\nvendor_id\t: AuthenticAMD\nmodel\t: 1\n",
    )
    assert host.cpu_vendor == guest.AMD_VENDOR
    assert host.missing_devices == ("/dev/net/tun",)


def test_an_unreadable_cpuinfo_leaves_the_vendor_unknown() -> None:
    host = guest.probe_host(platform="linux", device_exists=lambda _p: True, cpuinfo=lambda: "")
    assert host.cpu_vendor is None


# -- CPU cores --------------------------------------------------------------


def test_an_amd_host_is_pinned_to_one_core() -> None:
    """dockur/macos#268: multi-core on AMD is unstable, not merely slower."""
    assert guest.effective_cpu_cores(GuestSpec(), AMD_HOST) == 1


def test_an_unknown_vendor_takes_the_conservative_single_core_path() -> None:
    unknown = HostCapability(
        platform="linux", devices_present=guest.REQUIRED_DEVICES, cpu_vendor=None
    )
    assert guest.effective_cpu_cores(GuestSpec(), unknown) == 1


def test_an_intel_host_is_not_pinned_to_one_core() -> None:
    assert guest.effective_cpu_cores(GuestSpec(), INTEL_HOST) > 1


def test_an_explicit_core_count_overrides_the_vendor_rule() -> None:
    assert guest.effective_cpu_cores(GuestSpec(cpu_cores=4), AMD_HOST) == 4


# -- create arguments -------------------------------------------------------


def test_create_args_pass_through_both_devices_and_the_capability() -> None:
    args = guest.create_args(GuestSpec(), INTEL_HOST)
    assert args.count("--device") == 2
    for device in guest.REQUIRED_DEVICES:
        assert device in args
    assert "--cap-add" in args
    assert "NET_ADMIN" in args


def test_create_args_publish_ssh_and_the_web_console() -> None:
    args = guest.create_args(GuestSpec(ssh_port=2299, web_port=8010), INTEL_HOST)
    assert "2299:22" in args
    assert "8010:8006" in args


def test_create_args_use_a_long_stop_timeout() -> None:
    """A SIGKILL after the 10s default can tear a tens-of-GB guest filesystem."""
    args = guest.create_args(GuestSpec(), INTEL_HOST)
    assert args[args.index("--stop-timeout") + 1] == str(guest.STOP_TIMEOUT_SECONDS)


def test_create_args_carry_the_amd_core_limit_as_env() -> None:
    assert "CPU_CORES=1" in guest.create_args(GuestSpec(), AMD_HOST)


def test_create_args_carry_the_guest_sizing() -> None:
    args = guest.create_args(GuestSpec(version="sonoma", ram_size="16G"), INTEL_HOST)
    assert "VERSION=sonoma" in args
    assert "RAM_SIZE=16G" in args


# -- readiness --------------------------------------------------------------


class FakeClock:
    def __init__(self) -> None:
        self.value = 0.0

    def now(self) -> float:
        return self.value

    def sleep(self, seconds: float) -> None:
        self.value += seconds


def test_readiness_returns_as_soon_as_sshd_answers() -> None:
    clock = FakeClock()
    answers = iter([False, False, True])
    waited = guest.wait_for_sshd(
        GuestSpec(ready_poll_interval=10),
        probe=lambda _h, _p: next(answers),
        now=clock.now,
        sleep=clock.sleep,
    )
    assert waited == 20.0


def test_readiness_gives_up_at_the_deadline_rather_than_hanging() -> None:
    clock = FakeClock()
    with pytest.raises(GuestNotReadyError) as exc:
        guest.wait_for_sshd(
            GuestSpec(ready_timeout=60, ready_poll_interval=10),
            probe=lambda _h, _p: False,
            now=clock.now,
            sleep=clock.sleep,
        )
    # The web console is where a stalled first-boot installer is actually visible.
    assert "8006" in str(exc.value)
    assert clock.value >= 60


def test_a_guest_that_is_already_up_never_sleeps() -> None:
    clock = FakeClock()
    assert (
        guest.wait_for_sshd(
            GuestSpec(), probe=lambda _h, _p: True, now=clock.now, sleep=clock.sleep
        )
        == 0.0
    )
    assert clock.value == 0.0


def test_readiness_reports_progress_while_it_waits() -> None:
    clock = FakeClock()
    seen: list[str] = []
    answers = iter([False, True])
    guest.wait_for_sshd(
        GuestSpec(ready_poll_interval=5),
        probe=lambda _h, _p: next(answers),
        now=clock.now,
        sleep=clock.sleep,
        on_wait=seen.append,
    )
    assert seen and "sshd" in seen[0]


# -- transport --------------------------------------------------------------


def test_ssh_argv_targets_the_declared_port_and_user() -> None:
    argv = guest.ssh_argv(GuestSpec(ssh_port=2299, ssh_user="builder"), "true")
    assert argv[0] == "ssh"
    assert "2299" in argv
    assert "builder@127.0.0.1" in argv
    assert argv[-1] == "true"


def test_ssh_never_records_the_guest_host_key() -> None:
    """The guest is a throwaway VM on a reused loopback port; its key legitimately rolls."""
    argv = guest.ssh_argv(GuestSpec(), "true")
    assert "UserKnownHostsFile=/dev/null" in argv
    assert "StrictHostKeyChecking=no" in argv


def test_scp_argv_uses_the_uppercase_port_flag() -> None:
    # scp spells it -P; ssh spells it -p. Getting this wrong reads as a missing file.
    argv = guest.scp_argv(GuestSpec(ssh_port=2299), "/tmp/a.tar", "~/a.tar")
    assert argv[argv.index("-P") + 1] == "2299"


def test_a_remote_command_is_prefixed_by_a_quoted_workdir() -> None:
    remote = guest.remote_command(GuestSpec(), "make test", workdir="/Users/run ner")
    assert remote == "cd '/Users/run ner' && make test"


def test_a_remote_command_without_a_workdir_is_passed_through_verbatim() -> None:
    assert guest.remote_command(GuestSpec(), "a && b") == "a && b"


def test_run_remote_returns_the_guests_own_exit_code() -> None:
    def runner(argv, **_kwargs):
        assert argv[0] == "ssh"
        return subprocess.CompletedProcess(argv, 3, "out", "err")

    assert guest.run_remote(["ssh", "x"], runner=runner) == (3, "out\nerr")


def test_a_missing_ssh_client_is_a_clear_refusal_not_a_traceback() -> None:
    def runner(_argv, **_kwargs):
        raise FileNotFoundError("ssh")

    with pytest.raises(GuestUnsupportedError) as exc:
        guest.run_remote(["ssh", "x"], runner=runner)
    assert "on PATH" in str(exc.value)


def test_ssh_transport_failures_are_flagged_as_ambiguous() -> None:
    assert "connection failures" in guest.describe_exit(guest.SSH_TRANSPORT_FAILURE)
    assert "connection failures" not in guest.describe_exit(1)


def test_the_interactive_login_command_uses_a_real_login_shell() -> None:
    assert guest.login_command(None) == "exec $SHELL -l"
    assert guest.login_command("/repo") == "cd /repo && exec $SHELL -l"


# -- payload shipping -------------------------------------------------------


def test_no_payload_declared_ships_nothing(tmp_path: Path) -> None:
    def runner(_argv, **_kwargs):
        pytest.fail("nothing should be shipped when no payload is declared")

    assert guest.ship_payload(GuestSpec(), tmp_path, runner=runner) is None


def test_a_declared_payload_is_scp_ed_to_its_destination(tmp_path: Path) -> None:
    (tmp_path / "kernal-x64.tar.zst").write_bytes(b"archive")
    sent: list[list[str]] = []

    def runner(argv, **_kwargs):
        sent.append(argv)
        return subprocess.CompletedProcess(argv, 0, "", "")

    landed = guest.ship_payload(
        GuestSpec(payload="kernal-x64.tar.zst", payload_destination="~/a.tar.zst"),
        tmp_path,
        runner=runner,
    )
    assert landed == "~/a.tar.zst"
    assert sent[0][0] == "scp"
    assert sent[0][-1] == "runner@127.0.0.1:~/a.tar.zst"


def test_a_missing_payload_is_refused_before_the_task_runs(tmp_path: Path) -> None:
    """A guest silently serving last week's archive is the worst failure this kind has."""
    with pytest.raises(GuestUnsupportedError) as exc:
        guest.ship_payload(
            GuestSpec(payload="missing.tar"), tmp_path, runner=lambda *_a, **_k: None
        )
    assert "not a file" in str(exc.value)


def test_a_failed_ship_stops_the_task(tmp_path: Path) -> None:
    (tmp_path / "a.tar").write_bytes(b"x")

    def runner(argv, **_kwargs):
        return subprocess.CompletedProcess(argv, 1, "", "permission denied")

    with pytest.raises(GuestNotReadyError):
        guest.ship_payload(GuestSpec(payload="a.tar"), tmp_path, runner=runner)


def test_an_ssh_timeout_is_reported_as_a_guest_failure_not_a_traceback() -> None:
    def runner(_argv, **_kwargs):
        raise subprocess.TimeoutExpired("ssh", 1)

    with pytest.raises(GuestNotReadyError):
        guest.run_remote(["ssh", "x"], timeout=1, runner=runner)
    with pytest.raises(GuestNotReadyError):
        guest.interactive_remote(["ssh", "x"], timeout=1, runner=runner)


def test_interactive_remote_applies_the_run_deadline() -> None:
    seen: dict[str, object] = {}

    def runner(argv, **kwargs):
        seen.update(kwargs)
        return subprocess.CompletedProcess(argv, 0)

    guest.interactive_remote(["ssh", "x"], timeout=42, runner=runner)
    assert seen["timeout"] == 42


def test_guest_failures_are_engine_errors_so_every_cli_handler_catches_them() -> None:
    from bosn.engine import EngineError

    assert issubclass(GuestUnsupportedError, EngineError)
    assert issubclass(GuestNotReadyError, EngineError)


# -- the retention label survives a lost registry --------------------------


def test_a_pinned_volume_carries_its_tier_in_its_labels() -> None:
    """The registry is disposable; ownership -- and the tier -- live in the labels."""
    from bosn import labels

    pinned = labels.ResourceLabels(
        registry="r",
        kind="volume",
        stack="mac",
        generation="g",
        scope="machine",
        workspace="/w",
        created="now",
        retention="pinned",
    )
    raw = pinned.to_dict()
    assert raw[labels.RETENTION] == "pinned"
    assert labels.ResourceLabels.from_dict(raw).retention == "pinned"


def test_an_unpinned_volume_writes_exactly_the_labels_bosn_always_wrote() -> None:
    from bosn import labels

    warm = labels.ResourceLabels(
        registry="r",
        kind="volume",
        stack="s",
        generation="g",
        scope="stack",
        workspace="/w",
        created="now",
    )
    assert labels.RETENTION not in warm.to_dict()
    assert labels.is_complete(warm.to_dict())


def test_the_retention_label_is_not_required_for_ownership() -> None:
    """Every resource predating the tier must stay ours, collectable, and adoptable."""
    from bosn import labels

    assert labels.RETENTION not in labels.REQUIRED_LABELS
