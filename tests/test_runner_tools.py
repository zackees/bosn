"""Bosn supplies stock hosted-runner tools before executing workflows."""

import hashlib
import subprocess
import tarfile
import tempfile
import unittest
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


@dataclass(frozen=True)
class ToolArchive:
    name: str
    binary: str
    digest: str


ARCHIVES = (
    ToolArchive(
        "powershell", "pwsh", "ddbc4a2d113bbd46d283cfedcbcd117a70caefd7673f41f2b4e0000badf103bc"
    ),
    ToolArchive(
        "github-cli",
        "package/bin/gh",
        "bb766f710eef8ede859c18578c72c327597cd4c8a85b06001b1f3843c6019386",
    ),
)


class RunnerToolsTests(unittest.TestCase):
    def test_verified_install_is_reused_without_a_second_download(self):
        self.exercise_install(corrupt=False)

    def test_corrupt_archive_never_publishes_a_completed_generation(self):
        self.exercise_install(corrupt=True)

    def exercise_install(self, corrupt):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            script = (ROOT / "crates/bosn-service/src/ci/engine/runner_tools.sh").read_text()
            script = script.replace("/opt/hostedtoolcache", str(root / "cache"))
            script = script.replace("@TOOLS_ID@", "fixture")
            for fixture in ARCHIVES:
                binary = root / fixture.name / fixture.binary
                binary.parent.mkdir(parents=True)
                binary.write_text("#!/bin/sh\necho verified-tool\n")
                binary.chmod(0o755)
                archive_path = root / (fixture.name + ".tar.gz")
                with tarfile.open(archive_path, "w:gz") as archive:
                    archive.add(
                        root / fixture.name, arcname="." if fixture.name == "powershell" else ""
                    )
                actual = hashlib.sha256(archive_path.read_bytes()).hexdigest()
                script = script.replace(fixture.digest, "0" * 64 if corrupt else actual)
            fake_curl = root / "fake-curl"
            fake_curl.write_text(
                "#!/bin/bash\n"
                f'echo download >> "{root}/downloads"\n'
                'if [[ "$*" == *PowerShell* ]]; then tool=powershell; else tool=github-cli; fi\n'
                f'cp "{root}/$tool.tar.gz" "${{@: -1}}"\n'
            )
            fake_curl.chmod(0o755)
            script = script.replace("curl -fsSL", f"'{fake_curl}' -fsSL")
            install = root / "install"
            install.mkdir()
            script = script.replace("install=$(mktemp -d)", f"install='{install}'")
            with tempfile.TemporaryFile() as output:
                result = subprocess.run(
                    ["bash", "-ec", script], stdout=output, stderr=output, check=False
                )
                marker = root / "cache/bosn-runner-tools/fixture/.complete"
                if corrupt:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertFalse(marker.exists())
                    return
                output.seek(0)
                self.assertEqual(result.returncode, 0, output.read().decode())
                self.assertTrue(marker.exists())
                downloads = (root / "downloads").read_text()
                self.assertEqual(len(downloads.splitlines()), 2)
                second = subprocess.run(
                    ["bash", "-ec", script], stdout=output, stderr=output, check=False
                )
                self.assertEqual(second.returncode, 0)
                self.assertEqual((root / "downloads").read_text(), downloads)

    def test_tools_are_prepared_after_the_tool_cache_is_seeded(self):
        engine = (ROOT / "crates/bosn-service/src/ci/engine.rs").read_text()
        self.assertIn("runner_tools::prepare_script()", engine)
        self.assertIn("runner_tools::path_env()", engine)
        # The tool cache is seeded through `prepare_toolcache_script`, which
        # yields the legacy seed with no frozen generation and the native
        # overlay with one. Both must land before runner stock tools, because
        # those tools install into the tool cache the seed just populated.
        self.assertIn("prepare_toolcache_script(generation)?", engine)
        self.assertLess(
            engine.index("Self::exec(engine, &prepare_toolcache_script(generation)?)"),
            engine.index("runner_tools::prepare_script()"),
        )

    def test_archives_are_pinned_and_completion_follows_probes(self):
        script = (ROOT / "crates/bosn-service/src/ci/engine/runner_tools.sh").read_text()
        self.assertIn("sha256sum -c", script)
        self.assertLess(
            script.index('bin/pwsh" --version'), script.index('touch "$cache/.complete"')
        )
        self.assertLess(script.index('bin/gh" --version'), script.index('touch "$cache/.complete"'))


if __name__ == "__main__":
    unittest.main()
