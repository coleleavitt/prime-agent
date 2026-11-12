#!/usr/bin/env python3
"""Packaging regressions; real old-installer execution lives in test_legacy_upgrade."""

import argparse
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from assemble_artifacts import (
    BUNDLED_CATALOG_FILES,
    deny_allowed_licenses,
    deny_license_exceptions,
    pack_tarball,
    stage_tree,
    write_third_party_notices,
)
from native_compat import validate_compatibility_archive


class NativeCompatibilityTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "skills").mkdir()
        for name in ("LICENSE", "README.md"):
            (self.root / name).write_text("test fixture\n")
        runtime = self.root / "prime-agent-runtime"
        (runtime / "src/rlm").mkdir(parents=True)
        (runtime / "src/rlm/repl.py").write_text("# runtime fixture\n")
        (runtime / "pyproject.toml").write_text('[project]\nname = "fixture"\n')
        for name in BUNDLED_CATALOG_FILES:
            (self.root / name).write_text("{}\n")
        self.binary = self.root / "binary"
        self.binary.write_text("#!/bin/sh\nprintf '1.0.0\\n'\n")
        self.binary.chmod(0o755)

    def assemble(self, target="aarch64-apple-darwin", stamp=None):
        staging = self.root / "stage"
        staging.mkdir()
        args = argparse.Namespace(
            repo_root=self.root, target=target, binary=self.binary,
            runtime_dir=None, catalog_assets=self.root, version="1.0.0", sha="a" * 40,
        )
        # Catalog validation has its own integration suite and the notices need
        # a Cargo workspace; this test exercises packaging with a minimal
        # payload to isolate the migration regression.
        with patch("assemble_artifacts.validate_bundled_catalog_dir", return_value={}), \
             patch("assemble_artifacts.write_third_party_notices",
                   side_effect=lambda _root, _target, out: out.write_text("notices\n")):
            facts = stage_tree(staging, args, stamp)
        archive = self.root / "release.tar.gz"
        pack_tarball(staging, archive, facts["payload"])
        return staging, archive, facts

    def test_stable_archive_satisfies_old_installer_contract(self):
        staging, archive, _ = self.assemble()
        validate_compatibility_archive(archive)
        self.assertEqual(json.loads((staging / "package.json").read_text())["version"], "1.0.0")
        self.assertEqual((staging / "prime-agent").read_bytes(), self.binary.read_bytes())
        repo = Path(__file__).resolve().parents[2]
        self.assertEqual((staging / "install.sh").read_bytes(), (repo / "install-rust.sh").read_bytes())

    def test_continuous_stamp_survives_compatibility_packaging(self):
        stamp = "1.0.0-continuous." + "a" * 40
        staging, archive, _ = self.assemble(stamp=stamp)
        validate_compatibility_archive(archive)
        self.assertEqual(json.loads((staging / "package.json").read_text())["version"], stamp)

    def test_missing_legacy_file_rejects_archive(self):
        staging, archive, facts = self.assemble()
        (staging / "theme/prime.json").unlink()
        pack_tarball(staging, archive, facts["payload"])
        with self.assertRaisesRegex(ValueError, "theme/prime.json"):
            validate_compatibility_archive(archive)

    def test_placeholder_wasm_rejects_archive(self):
        staging, archive, facts = self.assemble()
        (staging / "photon_rs_bg.wasm").write_bytes(b"placeholder")
        pack_tarball(staging, archive, facts["payload"])
        with self.assertRaisesRegex(ValueError, "Photon.*checksum"):
            validate_compatibility_archive(archive)

    def test_windows_retains_common_metadata_without_posix_legacy_assets(self):
        staging, archive, _ = self.assemble(target="x86_64-pc-windows-msvc")
        validate_compatibility_archive(archive)
        self.assertTrue((staging / "prime-agent.exe").is_file())
        self.assertFalse((staging / "photon_rs_bg.wasm").exists())


class ThirdPartyNoticesTests(unittest.TestCase):
    def test_accepted_licenses_are_deny_toml_allow_list(self):
        import tomllib  # the release build hosts lack it (Python 3.9/3.10); the tests need 3.11+

        deny = Path(__file__).resolve().parents[2] / "deny.toml"
        licenses = tomllib.loads(deny.read_text())["licenses"]
        self.assertEqual(deny_allowed_licenses(deny), licenses["allow"])
        self.assertEqual(deny_license_exceptions(deny),
                         {entry["crate"]: entry["allow"] for entry in licenses.get("exceptions", [])})

    def test_notices_drop_terminal_control_characters(self):
        def cargo_about(command, **_):
            output = Path(command[command.index("--output-file") + 1])
            output.write_bytes("a\x1b[31mb\x07\tc\x9b\r\nd\x7f\n".encode())
            return subprocess.CompletedProcess(command, 0, "", "")

        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "notices.txt"
            with patch("assemble_artifacts.subprocess.run", side_effect=cargo_about):
                write_third_party_notices(Path(__file__).resolve().parents[2],
                                          "x86_64-unknown-linux-gnu", out)
            self.assertEqual(out.read_bytes(), b"a[31mb\tc\nd\n")


if __name__ == "__main__":
    unittest.main()
