"""The checkout host binary is found in cargo's build directory, wherever
`CARGO_TARGET_DIR` or a `.cargo/config.toml` (`build.target-dir`) puts it."""

from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from rlm import _host


class CargoTargetDirTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name).resolve()
        self.checkout = self.root / "work" / "checkout"
        self.checkout.mkdir(parents=True)
        self.cargo_home = self.root / "cargo-home"
        self.env = mock.patch.dict(os.environ, {"CARGO_HOME": str(self.cargo_home)})
        self.env.start()
        os.environ.pop("CARGO_TARGET_DIR", None)

    def tearDown(self) -> None:
        self.env.stop()
        self._tmp.cleanup()

    def config(self, directory: Path, body: str) -> None:
        (directory / ".cargo").mkdir(parents=True, exist_ok=True)
        (directory / ".cargo" / "config.toml").write_text(body, encoding="utf-8")

    def test_the_default_is_the_checkouts_target(self) -> None:
        self.assertEqual(_host.cargo_target_dir(self.checkout), self.checkout / "target")

    def test_the_nearest_config_wins_and_a_relative_dir_is_its_parents(self) -> None:
        self.config(self.root, '[build]\ntarget-dir = "shared/target"\n')
        self.assertEqual(_host.cargo_target_dir(self.checkout), self.root / "shared" / "target")
        self.config(self.checkout, '[build]\ntarget-dir = "/abs/target"\n')
        self.assertEqual(_host.cargo_target_dir(self.checkout), Path("/abs/target"))

    def test_cargo_home_config_and_the_environment(self) -> None:
        (self.cargo_home).mkdir(parents=True)
        (self.cargo_home / "config.toml").write_text('[build]\ntarget-dir = "/home-target"\n', encoding="utf-8")
        self.assertEqual(_host.cargo_target_dir(self.checkout), Path("/home-target"))
        with mock.patch.dict(os.environ, {"CARGO_TARGET_DIR": "out"}):
            self.assertEqual(_host.cargo_target_dir(self.checkout), self.checkout / "out")

    def test_a_config_without_a_target_dir_or_unparsable_is_skipped(self) -> None:
        self.config(self.checkout, "[build\n")
        self.config(self.root / "work", '[alias]\nb = "build"\n')
        self.config(self.root, '[build]\ntarget-dir = "t"\n')
        self.assertEqual(_host.cargo_target_dir(self.checkout), self.root / "t")


if __name__ == "__main__":
    unittest.main()
