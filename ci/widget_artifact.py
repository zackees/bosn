"""Stage and verify a source-bound Linux desktop release archive."""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import re
import subprocess
import tarfile
import tempfile
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import TypeAlias

TARGET = "x86_64-unknown-linux-gnu"
MAX_BINARY = 256 << 20
MAX_ARCHIVE = 256 << 20
JsonValue: TypeAlias = str | int | float | bool | None | list["JsonValue"] | dict[str, "JsonValue"]


def document_fields(document: str, fields: set[str]) -> dict[str, JsonValue]:
    value: JsonValue = json.loads(document)
    if not isinstance(value, dict) or set(value) != fields:
        raise ValueError("invalid widget metadata fields")
    return value


def string_field(value: dict[str, JsonValue], name: str) -> str:
    result = value[name]
    if not isinstance(result, str):
        raise ValueError("widget metadata fields must be strings")
    return result


@dataclass(frozen=True)
class BuildInfo:
    """Identity embedded in the executable before its desktop startup."""

    version: str
    source_sha: str
    target: str

    def __post_init__(self) -> None:
        if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", self.version):
            raise ValueError("invalid version")
        if not re.fullmatch(r"[0-9a-f]{40}", self.source_sha):
            raise ValueError("invalid source SHA")
        if self.target != TARGET:
            raise ValueError("unsupported widget target")

    @classmethod
    def from_json(cls, document: str) -> BuildInfo:
        value = document_fields(document, {"version", "source_sha", "target"})
        return cls(*(string_field(value, name) for name in ("version", "source_sha", "target")))


@dataclass(frozen=True)
class Manifest:
    """Archive contents and executable identity."""

    schema: int
    version: str
    source_sha: str
    target: str
    binary_sha256: str


@dataclass(frozen=True)
class ArchiveFile:
    name: str
    contents: bytes
    mode: int


def native_binary(data: bytes) -> None:
    """Reject scripts, other architectures and unbounded binary payloads."""
    if not 64 <= len(data) <= MAX_BINARY or data[:6] != b"\x7fELF\x02\x01":
        raise ValueError("widget must be a bounded 64-bit little-endian ELF")
    if int.from_bytes(data[18:20], "little") != 62:
        raise ValueError("widget ELF architecture must be x86_64")


def build_info(binary: Path) -> BuildInfo:
    """Read headless executable metadata without pipe-captured subprocess output."""
    with tempfile.TemporaryFile() as output:
        subprocess.run(
            [str(binary.resolve()), "--build-info"], check=True, stdout=output, timeout=10
        )
        output.seek(0)
        document = output.read(4097)
    if len(document) > 4096:
        raise ValueError("widget build metadata exceeds its limit")
    return BuildInfo.from_json(document.decode("utf-8"))


def checksum(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def stage(binary: Path, destination: Path, expected: BuildInfo) -> Path:
    """Package only the verified executable and its immutable manifest."""
    if binary.is_symlink() or not binary.is_file() or binary.stat().st_size > MAX_BINARY:
        raise ValueError("widget binary must be a bounded regular file")
    data = binary.read_bytes()
    native_binary(data)
    if build_info(binary) != expected:
        raise ValueError("widget build metadata differs from expected source/version")
    manifest = Manifest(1, expected.version, expected.source_sha, expected.target, checksum(data))
    metadata = json.dumps(asdict(manifest), sort_keys=True).encode("utf-8")
    destination.mkdir(parents=True, exist_ok=True)
    archive = destination / f"bosn-widget-v{expected.version}-{expected.target}.tar.gz"
    with tarfile.open(archive, "w:gz") as output:
        for entry in (
            ArchiveFile("bosn-widget", data, 0o755),
            ArchiveFile("manifest.json", metadata, 0o644),
        ):
            member = tarfile.TarInfo(entry.name)
            member.size = len(entry.contents)
            member.mode = entry.mode
            output.addfile(member, io.BytesIO(entry.contents))
    archive.with_suffix(archive.suffix + ".sha256").write_text(
        f"{checksum(archive.read_bytes())}  {archive.name}\n", encoding="utf-8"
    )
    verify(archive, expected)
    return archive


@dataclass(frozen=True)
class ArchivePayload:
    binary: bytes
    metadata: bytes


def read_payload(raw: bytes) -> ArchivePayload:
    """Read two regular members without scanning an unbounded archive."""
    with tarfile.open(fileobj=io.BytesIO(raw), mode="r:gz") as contents:
        members = []
        for member in contents:
            if len(members) == 2:
                raise ValueError("too many widget archive members")
            ceiling = 4096 if member.name == "manifest.json" else MAX_BINARY
            if not member.isfile() or not 0 <= member.size <= ceiling:
                raise ValueError("unsafe widget archive member")
            members.append(member)
        if len(members) != 2 or {member.name for member in members} != {
            "bosn-widget",
            "manifest.json",
        }:
            raise ValueError("unexpected widget archive members")
        binary_file = contents.extractfile("bosn-widget")
        manifest_file = contents.extractfile("manifest.json")
        if binary_file is None or manifest_file is None:
            raise ValueError("missing widget archive member")
        binary = binary_file.read(MAX_BINARY + 1)
        metadata = manifest_file.read(4097)
    return ArchivePayload(binary, metadata)


def verify(archive: Path, expected: BuildInfo) -> BuildInfo:
    """Fail closed on identity, checksum, member type, size or payload drift."""
    if archive.is_symlink() or not archive.is_file() or archive.stat().st_size > MAX_ARCHIVE:
        raise ValueError("invalid bounded widget archive")
    raw = archive.read_bytes()
    sidecar = archive.with_suffix(archive.suffix + ".sha256")
    if sidecar.is_symlink() or sidecar.stat().st_size > 256:
        raise ValueError("invalid widget checksum sidecar")
    if sidecar.read_text(encoding="utf-8") != f"{checksum(raw)}  {archive.name}\n":
        raise ValueError("widget archive checksum mismatch")
    payload = read_payload(raw)
    binary = payload.binary
    metadata = payload.metadata
    native_binary(binary)
    if len(metadata) > 4096:
        raise ValueError("widget manifest exceeds its limit")
    value = document_fields(metadata.decode("utf-8"), set(Manifest.__dataclass_fields__))
    schema = value["schema"]
    if type(schema) is not int:
        raise ValueError("widget manifest schema must be an integer")
    manifest = Manifest(
        schema,
        *(
            string_field(value, name)
            for name in ("version", "source_sha", "target", "binary_sha256")
        ),
    )
    identity = BuildInfo(manifest.version, manifest.source_sha, manifest.target)
    if manifest.schema != 1 or identity != expected:
        raise ValueError("widget source/version/target mismatch")
    if manifest.binary_sha256 != checksum(binary):
        raise ValueError("widget binary checksum mismatch")
    return identity


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("stage", "verify"))
    parser.add_argument("path", type=Path)
    parser.add_argument("--destination", type=Path, default=Path("widget-dist"))
    parser.add_argument("--version", required=True)
    parser.add_argument("--source-sha", required=True)
    args = parser.parse_args()
    identity = BuildInfo(args.version, args.source_sha, TARGET)
    if args.operation == "stage":
        print(stage(args.path, args.destination, identity))
    else:
        verify(args.path, identity)


if __name__ == "__main__":
    main()
