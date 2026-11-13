#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

"""Tests of mode/nativelink_preflight.py: `python3 -m unittest mode/test_nativelink_preflight.py`.

Each test builds a small project with the real mode file, startup config and startup file (the
root .buckconfig.local a run copies from mode/nativelink-startup.buckconfig), a prelude, a manifest
whose roots exist, and an engine. The complete invocation passes; every omission, partial mode,
wrong pin or wrong input is refused, and the launcher never runs.
"""

import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
PREFLIGHT = HERE / "nativelink_preflight.py"
TREE = HERE.parent


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class Project:
    def __init__(self, base: Path):
        self.root = base / "project"
        (self.root / "mode").mkdir(parents=True)
        shutil.copy(TREE / "mode/nativelink.buckconfig", self.root / "mode")
        shutil.copy(TREE / "mode/nativelink-startup.buckconfig", self.root / "mode")
        shutil.copy(TREE / "mode/nativelink-startup.buckconfig", self.root / ".buckconfig.local")
        shutil.copy(TREE / ".buckconfig", self.root)
        (self.root / ".buckconfig.d").mkdir()
        shutil.copy(TREE / ".buckconfig.d/common.buckconfig", self.root / ".buckconfig.d")
        # A home of its own, so the developer's ~/.buckconfig.local cannot decide a test.
        self.home = base / "home"
        self.home.mkdir()
        self.prelude = base / "prelude"
        (self.prelude / "rust").mkdir(parents=True)
        (self.prelude / "rust/remote_case_parsers.bzl").write_text("def parse(): pass\n")
        (self.root / ".buck2").mkdir()
        os.symlink(self.prelude, self.root / ".buck2/prelude-source")
        self.store_root = base / "store-root"
        self.store_root.mkdir()
        (self.root / "shim/nix").mkdir(parents=True)
        self.manifest = self.root / "shim/nix/nix-manifest.json"
        self.manifest.write_text(json.dumps({"roots": [{"path": str(self.store_root)}]}))
        # The engine is a script that records that it ran; no check may let it run on refusal.
        self.ran = base / "ran"
        self.engine = base / "engine"
        self.engine.write_text(f"#!/bin/sh\necho ran > {self.ran}\n")
        self.engine.chmod(0o755)
        self.digest = self.tree_digest(self.prelude)
        self.nix_digest = self.tree_digest(self.root / "shim/nix")
        self.config_digest = subprocess.run(
            [sys.executable, PREFLIGHT, "--print-config-digest"],
            cwd=self.root,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()

    @staticmethod
    def tree_digest(path: Path) -> str:
        return subprocess.run(
            [sys.executable, PREFLIGHT, "--print-prelude-digest", path],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()

    def daemon_dir(self, isolation: str = "nl") -> Path:
        """Where buck2 keeps the daemon of this project and isolation, under the test's home."""
        return self.home / ".buck/buckd" / self.root.relative_to(self.root.anchor) / isolation

    def record(self, isolation: str = "nl") -> Path:
        return self.root / ".buck2/nativelink-startup" / f"{isolation}.json"

    def env(self, **extra) -> dict[str, str]:
        # The caller's own BUCK* variables would be refused; each test sets the ones it means.
        inherited = {k: v for k, v in os.environ.items() if not k.startswith("BUCK")}
        return {**inherited, "HOME": str(self.home), **extra}

    def pins(self, **override) -> list[str]:
        pins = {
            "--engine": str(self.engine),
            "--engine-sha256": sha(self.engine),
            "--launcher-sha256": sha(self.engine),
            "--prelude-digest": self.digest,
            "--manifest-sha256": sha(self.manifest),
            "--nix-cell-digest": self.nix_digest,
            "--config-sha256": self.config_digest,
        }
        pins.update(override)
        return [word for item in pins.items() for word in item if item[1] is not None]

    def run(self, buck2_args, pins=None, extra=(), env=None):
        result = subprocess.run(
            [sys.executable, PREFLIGHT, *(self.pins() if pins is None else pins), *extra, "--"]
            + [str(self.engine), *buck2_args],
            cwd=self.root,
            capture_output=True,
            text=True,
            env=self.env() if env is None else env,
        )
        return result.returncode, result.stderr


BUILD = [
    "--isolation-dir",
    "nl",
    "build",
    "--remote-only",
    "--config-file",
    "mode/nativelink.buckconfig",
    "gh_facebook_buck2//app/buck2:buck2-bin",
]
TEST = [
    "--isolation-dir",
    "nl",
    "test",
    "--remote-only",
    "--config-file",
    "mode/nativelink.buckconfig",
    "-c",
    "test.use_internal_runner=true",
    "root//app/buck2_test:buck2_test-unittest",
]


class NativeLinkPreflightTest(unittest.TestCase):
    def setUp(self):
        self.base = Path(tempfile.mkdtemp(prefix="nativelink-preflight-"))
        self.addCleanup(shutil.rmtree, self.base)
        self.project = Project(self.base)

    def refused(self, buck2_args, expected, **kwargs):
        code, stderr = self.project.run(buck2_args, **kwargs)
        self.assertEqual(code, 2, stderr)
        self.assertIn(expected, stderr)
        self.assertIn("nothing was launched", stderr)
        self.assertFalse(self.project.ran.exists(), "the launcher ran on a refusal")

    def test_complete_build_and_test_launch_the_engine(self):
        for args in (BUILD, TEST):
            self.project.ran.unlink(missing_ok=True)
            code, stderr = self.project.run(args)
            self.assertEqual(code, 0, stderr)
            self.assertTrue(self.project.ran.exists())

    def test_check_only_does_not_launch(self):
        code, stderr = self.project.run(BUILD, extra=["--check-only"])
        self.assertEqual(code, 0, stderr)
        self.assertFalse(self.project.ran.exists())

    def test_an_omitted_or_replaced_mode_file_is_refused(self):
        omitted = [a for a in BUILD if a not in ("--config-file", "mode/nativelink.buckconfig")]
        self.refused(omitted, "--config-file must be exactly one mode/nativelink.buckconfig")
        self.refused(BUILD + ["--config-file", "other.buckconfig"], "--config-file must be")
        self.refused(BUILD + ["--config-file=mode/validate.buckconfig"], "closed-port")

    def test_a_partial_invocation_is_refused(self):
        self.refused([a for a in BUILD if a != "--remote-only"], "--remote-only is missing")
        self.refused(BUILD[2:], "--isolation-dir is missing")
        no_runner = [a for a in TEST if a not in ("-c", "test.use_internal_runner=true")]
        self.refused(no_runner, "-c/--config must be exactly ['test.use_internal_runner=true']")
        self.refused(BUILD + ["-c", "shim.toolchain_mode="], "-c/--config must be exactly []")
        self.refused(BUILD + ["-cbuild.execution_platforms=x"], "-c/--config must be exactly []")
        self.refused(BUILD + ["--prefer-local"], "--prefer-local is not allowed")
        self.refused(BUILD + ["--target-platforms", "x"], "--target-platforms is not allowed")
        self.refused(["--isolation-dir", "nl", "run"] + BUILD[3:], "not build or test")

    def test_indirect_argument_files_are_refused(self):
        # buck2 expands both forms before parsing; this file would replace the mode.
        (self.project.root / "other-args").write_text("--config-file\nwrong.buckconfig\n")
        for indirect in (["@other-args"], ["--flagfile", "other-args"], ["--flagfile=other-args"]):
            self.refused(BUILD + indirect, "indirect argument files (@FILE, --flagfile) are refused")
            self.refused(TEST + indirect, "indirect argument files (@FILE, --flagfile) are refused")

    def test_the_child_s_nonzero_exit_is_the_entry_s_exit(self):
        failing = self.base / "failing-engine"
        failing.write_text(f"#!/bin/sh\necho ran > {self.project.ran}\nexit 7\n")
        failing.chmod(0o755)
        pins = self.project.pins(
            **{"--engine": str(failing), "--engine-sha256": sha(failing), "--launcher-sha256": sha(failing)}
        )
        for args in (BUILD, TEST):
            self.project.ran.unlink(missing_ok=True)
            result = subprocess.run(
                [sys.executable, PREFLIGHT, *pins, "--", str(failing), *args],
                cwd=self.project.root,
                capture_output=True,
                text=True,
                env=self.project.env(),
            )
            self.assertEqual(result.returncode, 7, result.stderr)
            self.assertTrue(self.project.ran.exists())

    def test_a_partial_mode_file_or_startup_config_is_refused(self):
        mode = self.project.root / "mode/nativelink.buckconfig"
        original = mode.read_text()
        mode.write_text(original.replace("toolchain_mode = nativelink", "toolchain_mode = other"))
        self.refused(BUILD, "needs shim.toolchain_mode = nativelink, but it is 'other'")
        mode.write_text(original)
        startup = self.project.root / ".buckconfig"
        startup.write_text(startup.read_text().replace(", \\\n    prelude", ""))
        self.refused(BUILD, "project.ignore does not list prelude")

    def test_an_include_in_the_mode_file_or_startup_config_is_refused(self):
        (self.project.root / "other.buckconfig").write_text("[buck2_re_client]\ntls = true\n")
        for name in ("mode/nativelink.buckconfig", ".buckconfig"):
            path = self.project.root / name
            original = path.read_text()
            path.write_text(original + "<file:other.buckconfig>\n")
            self.refused(BUILD, "includes another file (<file:other.buckconfig>), which is refused")
            path.write_text(original + "<?file:other.buckconfig>\n")
            self.refused(BUILD, "includes another file (<?file:other.buckconfig>), which is refused")
            path.write_text(original)

    def test_other_configuration_inputs_are_refused(self):
        # CI-MODE-CONFIG-01: buck2 reads .buckconfig.local after .buckconfig, so anything but the
        # startup file there would change the mode's cells or startup ignore without any flag.
        root = self.project.root
        (root / "unaccepted-tools").mkdir()
        local = root / ".buckconfig.local"
        template = (root / "mode/nativelink-startup.buckconfig").read_text()
        for content in ("[cells]\nnix_toolchains = unaccepted-tools\n", "[project]\nignore =\n"):
            local.write_text(template + content)
            for args in (BUILD, TEST):
                self.refused(args, ".buckconfig.local is not exactly mode/nativelink-startup.buckconfig")
        local.write_text(template)
        extra = root / ".buckconfig.d/other.buckconfig"
        extra.write_text("[cells]\nnix_toolchains = unaccepted-tools\n")
        self.refused(BUILD, ".buckconfig.d must hold exactly ['.buckconfig.d/common.buckconfig']")
        extra.unlink()
        startup = root / ".buckconfig"
        original = startup.read_text()
        startup.write_text(original + "\n[cells]\nnix_toolchains = unaccepted-tools\n")
        self.refused(BUILD, "the root cells must be exactly")
        startup.write_text(original)
        (root / "shim").mkdir(exist_ok=True)
        for cell_local in (root / "shim/.buckconfig.local", self.project.prelude / ".buckconfig.local"):
            cell_local.write_text("[buck2_re_client]\ntls = true\n")
            self.refused(TEST, "which is not an accepted input")
            cell_local.unlink()
        (self.project.home / ".buckconfig.local").write_text("[cells]\nnix_toolchains = x\n")
        self.refused(BUILD, "~/.buckconfig.local exists")
        (self.project.home / ".buckconfig.local").unlink()
        code, stderr = self.project.run(BUILD)
        self.assertEqual(code, 0, stderr)

    def test_the_startup_file_must_be_exactly_the_template(self):
        # The daemon reads the remote execution client's settings only from the root project
        # configuration, never from --config-file, so they come from the run's .buckconfig.local.
        root = self.project.root
        local = root / ".buckconfig.local"
        template = root / "mode/nativelink-startup.buckconfig"
        original = template.read_text()
        local.unlink()
        for args in (BUILD, TEST):
            self.refused(args, ".buckconfig.local is missing")
        local.write_text(original.replace("grpc://127.0.0.1:50051", "grpc://127.0.0.1:1", 1))
        self.refused(BUILD, ".buckconfig.local is not exactly mode/nativelink-startup.buckconfig")
        local.unlink()
        local.symlink_to("mode/nativelink-startup.buckconfig")
        self.refused(BUILD, ".buckconfig.local is not a regular file")
        local.unlink()
        # A wrong endpoint in both the template and its copy: the root reader sees it, and the
        # configuration digest no longer matches.
        wrong = original.replace("engine_address = grpc://127.0.0.1:50051", "engine_address = grpc://127.0.0.1:1")
        template.write_text(wrong)
        local.write_text(wrong)
        self.refused(BUILD, "the startup configuration needs buck2_re_client.engine_address = grpc://127.0.0.1:50051")
        self.refused(BUILD, "have digest")
        template.write_text(original)
        local.write_text(original)
        code, stderr = self.project.run(BUILD)
        self.assertEqual(code, 0, stderr)

    def test_startup_settings_in_the_mode_file_are_refused(self):
        # The startup settings moved into the mode file would have no effect there.
        root = self.project.root
        mode = root / "mode/nativelink.buckconfig"
        template = root / "mode/nativelink-startup.buckconfig"
        local = root / ".buckconfig.local"
        mode_text, startup_text = mode.read_text(), template.read_text()
        for extra in (
            "[buck2_re_client]\nengine_address = grpc://127.0.0.1:50051\n",
            "[buck2]\ndigest_algorithms = SHA256\n",
            "[buck2_resource_control]\nstatus = off\n",
        ):
            mode.write_text(mode_text + "\n" + extra)
            section_key = extra.split("]")[0][1:] + "." + extra.split("\n")[1].split(" =")[0]
            self.refused(BUILD, f"sets {section_key}, which buck2 reads only at startup")
        # Moved out of the root: the startup file without them, the mode file with them.
        mode.write_text(mode_text + "\n" + startup_text)
        emptied = "# no startup settings\n"
        template.write_text(emptied)
        local.write_text(emptied)
        self.refused(TEST, "the startup configuration needs buck2_re_client.engine_address")
        self.refused(TEST, "which buck2 reads only at startup")
        mode.write_text(mode_text)
        template.write_text(startup_text)
        local.write_text(startup_text)
        code, stderr = self.project.run(TEST)
        self.assertEqual(code, 0, stderr)

    def test_an_isolation_s_daemons_must_have_read_the_bound_startup_inputs(self):
        project = self.project
        daemon, record = project.daemon_dir(), project.record()
        # A check does not start a daemon, so it records nothing.
        code, stderr = project.run(BUILD, extra=["--check-only"])
        self.assertEqual(code, 0, stderr)
        self.assertFalse(record.exists())
        # An existing daemon with no record of what it read is refused.
        daemon.mkdir(parents=True)
        for args in (BUILD, TEST):
            self.refused(args, "already has a daemon directory")
        daemon.rmdir()
        # A first launch in a new isolation records the startup inputs; the daemon it starts then
        # serves the TEST after the BUILD and a repeated TEST.
        code, stderr = project.run(BUILD)
        self.assertEqual(code, 0, stderr)
        self.assertTrue(record.exists())
        daemon.mkdir(parents=True)
        for args in (TEST, TEST):
            project.ran.unlink(missing_ok=True)
            code, stderr = project.run(args)
            self.assertEqual(code, 0, stderr)
            self.assertTrue(project.ran.exists())
        # A startup input replaced since the first launch, even by the same bytes, could have
        # been read differently by a daemon started meanwhile.
        local = project.root / ".buckconfig.local"
        replacement = project.root / "replacement"
        replacement.write_bytes(local.read_bytes())
        replacement.replace(local)
        project.ran.unlink()
        self.refused(TEST, "already has a daemon directory")
        # Another isolation is new, and qualifies afresh.
        other = [a if a != "nl" else "nl2" for a in TEST]
        code, stderr = project.run(other)
        self.assertEqual(code, 0, stderr)
        self.assertTrue(project.record("nl2").exists())

    def test_an_isolation_name_buck2_refuses_is_refused_before_any_write(self):
        # The isolation name also names the entry's own record, so a name buck2 would refuse is
        # refused before anything is written or launched, and is never normalized.
        project = self.project
        outside = self.base / "outside"
        outside.mkdir()
        before = sorted(p.relative_to(self.base) for p in self.base.rglob("*"))
        for bad in ("../outside/x", str(outside / "x"), ".", "..", "a/b", "a\\b", "._buck2-x"):
            for args in (BUILD, TEST):
                bad_args = [bad if a == "nl" else a for a in args]
                self.refused(bad_args, f"--isolation-dir {bad!r}")
        self.assertEqual(sorted(p.relative_to(self.base) for p in self.base.rglob("*")), before)
        self.assertFalse((project.root / ".buck2/nativelink-startup").exists())

    def test_the_startup_record_is_never_written_through_a_link(self):
        project = self.project
        outside = self.base / "outside"
        outside.mkdir()
        records = project.root / ".buck2/nativelink-startup"
        # The records directory as a link leading outside the tree.
        records.symlink_to(outside)
        for args in (BUILD, TEST):
            self.refused(args, "is not a directory (a link is refused)")
        self.assertEqual(list(outside.iterdir()), [])
        records.unlink()
        # The record itself as a link to a file outside the tree.
        records.mkdir()
        target = outside / "target"
        target.write_text("unchanged\n")
        (records / "nl.json").symlink_to(target)
        for args in (BUILD, TEST):
            self.refused(args, "is not a regular file (a link is refused)")
        self.assertEqual(target.read_text(), "unchanged\n")
        (records / "nl.json").unlink()
        code, stderr = project.run(BUILD)
        self.assertEqual(code, 0, stderr)
        self.assertTrue(stat.S_ISREG((records / "nl.json").lstat().st_mode))

    def test_a_cell_the_mode_file_redirects_is_refused(self):
        # CI-MODE-CONFIG-01, effective-cell class: buck2 applies the mode after the startup
        # configuration, so a cell added to the mode file replaces the startup one.
        (self.project.root / "unaccepted-tools").mkdir()
        mode = self.project.root / "mode/nativelink.buckconfig"
        original = mode.read_text()
        mode.write_text(original + "\n[cells]\nnix_toolchains = unaccepted-tools\n")
        for args in (BUILD, TEST):
            self.refused(args, "the cells in effect must be exactly")
            self.refused(args, "have digest")
        # Any other byte change to an accepted configuration input is refused by its digest.
        mode.write_text(original + "\n# a comment\n")
        self.refused(BUILD, "have digest")
        mode.write_text(original)
        code, stderr = self.project.run(BUILD)
        self.assertEqual(code, 0, stderr)

    def test_buck_configuration_in_the_environment_is_refused(self):
        # CI-MODE-ENV-01: buck2 appends BUCK2_TEST_EXTRA_EXTERNAL_CONFIG's file to its configuration.
        extra = self.base / "extra.buckconfig"
        extra.write_text("[external_cells]\nprelude = bundled\n")
        for name, value in (
            ("BUCK2_TEST_EXTRA_EXTERNAL_CONFIG", str(extra)),
            ("BUCK2_SETTINGS_OVERRIDE", "x"),
            ("BUCK2_TEST_SETTINGS_HOME_DIR", str(self.base)),
            ("BUCK_ANYTHING", "x"),
        ):
            for args in (BUILD, TEST):
                self.refused(args, f"the environment sets {name} (value not shown)", env=self.project.env(**{name: value}))
        # The engine's own name for a wrapper stays allowed.
        code, stderr = self.project.run(BUILD, env=self.project.env(MUNINN_BUCK2_REAL=str(self.project.engine)))
        self.assertEqual(code, 0, stderr)

    def test_a_changed_nix_toolchains_cell_is_refused(self):
        (self.project.root / "shim/nix/BUCK").write_text("# another toolchain\n")
        self.refused(TEST, "the nix_toolchains cell shim/nix has digest")

    def test_a_wrong_or_missing_prelude_is_refused(self):
        (self.project.prelude / "rust/remote_case_parsers.bzl").write_text("def other(): pass\n")
        self.refused(BUILD, "not the accepted")
        (self.project.root / ".buck2/prelude-source").unlink()
        self.refused(BUILD, "does not resolve to a directory")

    def test_a_wrong_manifest_or_missing_root_is_refused(self):
        self.refused(BUILD, "not the accepted one", pins=self.project.pins(**{"--manifest-sha256": "0" * 64}))
        self.project.store_root.rmdir()
        self.refused(BUILD, "does not exist")

    def test_a_wrong_engine_or_launcher_is_refused(self):
        self.refused(BUILD, "the engine", pins=self.project.pins(**{"--engine-sha256": "0" * 64}))
        self.refused(BUILD, "the launcher", pins=self.project.pins(**{"--launcher-sha256": "0" * 64}))

    def test_a_wrapper_must_name_the_engine(self):
        wrapper = self.base / "wrapper"
        wrapper.write_text('#!/bin/sh\nexec "$MUNINN_BUCK2_REAL" "$@"\n')
        wrapper.chmod(0o755)
        pins = self.project.pins(**{"--launcher-sha256": sha(wrapper)})

        def run(env_engine):
            env = self.project.env()
            env.pop("MUNINN_BUCK2_REAL", None)
            if env_engine is not None:
                env["MUNINN_BUCK2_REAL"] = env_engine
            return subprocess.run(
                [sys.executable, PREFLIGHT, *pins, "--", str(wrapper), *BUILD],
                cwd=self.project.root,
                capture_output=True,
                text=True,
                env={**env, "HOME": str(self.project.home)},
            )

        refused = run(None)
        self.assertEqual(refused.returncode, 2, refused.stderr)
        self.assertIn("MUNINN_BUCK2_REAL must name", refused.stderr)
        self.assertFalse(self.project.ran.exists())
        launched = run(str(self.project.engine))
        self.assertEqual(launched.returncode, 0, launched.stderr)
        self.assertTrue(self.project.ran.exists())

    def test_missing_pins_are_refused(self):
        self.refused(BUILD, "--prelude-digest is required", pins=self.project.pins(**{"--prelude-digest": None}))


if __name__ == "__main__":
    unittest.main()
