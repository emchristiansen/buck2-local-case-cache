# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

"""The single writer of a Rust test's Cargo-style runtime path environment.

A target generator supplies only per-crate values, as `rust_test` attributes:

- `cargo_manifest_dir`: the crate root, a project-relative path (default: the
  target's package);
- `cargo_bin_exes`: Cargo binary name -> binary target, one entry per
  `CARGO_BIN_EXE_<name>`.

From these, this module writes the test's runtime entries:

| Variable                    | Value                    | Kind  |
|-----------------------------|--------------------------|-------|
| `CARGO_MANIFEST_DIR`        | `cargo_manifest_dir`     | path  |
| `METACORTEX_WORKSPACE_ROOT` | `.`                      | path  |
| `INSTA_WORKSPACE_ROOT`      | `.` (the project root)   | path  |
| `INSTA_UPDATE`              | `no`                     | value |
| `CARGO_BIN_EXE_<name>`      | the binary's output      | path  |

A test that runs remotely also gets a worker-local `TMPDIR`, `.` (a path), and a
`HOME` the runner makes: one private directory per run beneath the account's
passwd home (`<pw_dir>/.l/XXXXXXXX`), named by `--account-home-dir-env HOME` and
removed when the run ends. That `HOME` is outside the action, on purpose: a
project-relative one is the action's work directory, too long for a Unix socket a
fixture binds beneath `HOME` to fit `sun_path`. The runner's own listing does not
receive it.

Each path entry is project-relative in the test's environment and is named once
by a leading `--project-relative-env NAME` runner option. The per-test process
runner (`RustToolchainInfo.test_process_runner`) makes it absolute against the
action's project root before any test or listing sees it, and refuses an unset,
empty, absolute or escaping value. The entries are therefore written only for a
libtest test (`framework = True`) that runs under that runner.

No other writer may set them: a target that names any of them in `run_env`, any
`CARGO_BIN_EXE_*` in `env`, or any of them in a `--project-relative-env` of its
`args`, fails analysis. On a remote test the same holds for `HOME` and `TMPDIR`,
in `env` as well as `run_env`. `env` may still carry a compile-time
`CARGO_MANIFEST_DIR` for rustc; the runtime value above replaces it in the
test's environment.

Every source file in `srcs` is also an input of the test action, at its
project path, as in the checkout Cargo runs tests in. A test that reads its
crate's sources (a directory listing, a file's text) reads the real files, and
a path a test builds from `file!()`, such as insta's
`<root>/<dir of file!()>/../snapshots`, resolves through real directories to
the declared resources it names. No directory is fabricated: one exists in the
action only because a declared input lies under it.

At compile time, a test under the runner also gets `CARGO_TARGET_TMPDIR`, the
project-relative `buck-out/cargo-target-tmp`, which the runner creates (one
`--project-relative-dir`) before any test runs. Cargo sets it only for
integration tests and benches; here every runner test has it, a superset. No
target may set it.

At compile time, `CARGO_BIN_EXE_<name>` is the binary's project-relative path,
without making the binary a compile input, so `env!`/`option_env!` sites
compile. That value is not absolute, unlike Cargo's; tests must take the path
from the runtime variable.
"""

load("@prelude//rust:rust_toolchain.bzl", "RustToolchainInfo")

# A directory of programs a test may run by name, each an exact store path the
# declared runtime manifest holds. A toolchain rule provides it; `nix_manifest` is
# that manifest, which must be the rust toolchain's own.
RuntimeToolsInfo = provider(fields = {
    "directory": provider_field(Artifact),
    "nix_manifest": provider_field(Artifact),
})

PROJECT_RELATIVE_ENV = "--project-relative-env"
CARGO_BIN_EXE_PREFIX = "CARGO_BIN_EXE_"

_FIXED_PATHS = ["CARGO_MANIFEST_DIR", "INSTA_WORKSPACE_ROOT", "METACORTEX_WORKSPACE_ROOT"]

# Cargo's per-target scratch directory for integration tests (`target/tmp`), as a
# project-relative compile-time value; the runner creates it before any test runs.
CARGO_TARGET_TMPDIR = "CARGO_TARGET_TMPDIR"
CARGO_TARGET_TMPDIR_PATH = "buck-out/cargo-target-tmp"
PROJECT_RELATIVE_DIR = "--project-relative-dir"
_FIXED_VALUES = {"INSTA_UPDATE": "no"}
_REMOTE_PATHS = ["TMPDIR"]

# A remote test's HOME: one private directory per run beneath the account's passwd home
# (`<pw_dir>/.l/XXXXXXXX`), which the runner makes and removes. A project-relative HOME
# would be the action's work directory, too long for a Unix socket a fixture binds beneath
# HOME to fit sun_path's 107 bytes. The test runtime contract (linux-unprivileged-v2)
# does not yet promise that such a directory can be made there or that the passwd home is
# short; until a version that does, the runner's named refusal is the enforcement.
_REMOTE_ACCOUNT_HOME = ["HOME"]
ACCOUNT_HOME_DIR_ENV = "--account-home-dir-env"
_BIN_NAME_CHARS = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-"

def cargo_test_runtime_attrs() -> dict[str, Attr]:
    return {
        "cargo_bin_exes": attrs.dict(
            key = attrs.string(),
            value = attrs.dep(providers = [RunInfo]),
            sorted = True,
            default = {},
            doc = """
                Cargo binary name -> binary target. The prelude writes
                `CARGO_BIN_EXE_<name>`: an absolute path at run time and a
                project-relative path at compile time. Never put
                `CARGO_BIN_EXE_*` into `env` or `run_env`.
            """,
        ),
        "runtime_tools": attrs.option(
            attrs.dep(providers = [RuntimeToolsInfo]),
            default = None,
            doc = """
                A directory of programs the test runs by name, provided by a
                toolchain rule as `RuntimeToolsInfo`, bound to the rust
                toolchain's declared runtime manifest. The test's PATH is then
                exactly that directory; the directory and the manifest are
                inputs of the test action. Without it, the prelude sets no PATH.
                It requires the libtest process runner; a test without the
                runner that names it fails analysis.
            """,
        ),
        "extra_resources": attrs.list(
            attrs.source(allow_directory = True),
            default = [],
            doc = """
                Files outside the target's package that the test reads at run
                time, as labels. Each is an input of the test action at its
                own path: a source file or directory (e.g. an `export_file`
                with `mode = "reference"`) at its project path. A directory
                brings its whole content, which a test that lists it or
                asserts that it exists needs.
            """,
        ),
        "source_resource_sets": attrs.list(
            attrs.dep(),
            default = [],
            doc = """
                Sets of source files outside the target's package that the test
                reads at run time, each a target whose default outputs are all
                source artifacts (for example `export_file` with
                `mode = "reference"`, or a rule returning its `attrs.source`
                inputs unchanged). Every file is an input of the test action at
                its own project path, as `extra_resources` are; a set lets one
                label bring many files, such as a Cargo workspace's read set. A
                set with any built output fails analysis, since a built output
                lies under buck-out, not at a project path.
            """,
        ),
        "cargo_manifest_dir": attrs.option(
            attrs.string(),
            default = None,
            doc = """
                The crate root as a project-relative path, which the prelude
                writes as the test's runtime `CARGO_MANIFEST_DIR` (absolute at
                run time). Defaults to the target's package.
            """,
        ),
    }

def _check_bin_name(label: Label, name: str):
    if not name or any([c not in _BIN_NAME_CHARS for c in name.elems()]):
        fail("{}: cargo_bin_exes key {} is not a Cargo binary name".format(label, repr(name)))

def _normalized_project_path(label: Label, value: str) -> str:
    """`value` as a canonical project-relative path, refusing absolute or escaping ones."""
    if not value:
        fail("{}: cargo_manifest_dir is empty".format(label))
    if value.startswith("/"):
        fail("{}: cargo_manifest_dir is absolute: {}".format(label, repr(value)))
    components = []
    for component in value.split("/"):
        if component == "" or component == ".":
            continue
        if component == "..":
            if not components:
                fail("{}: cargo_manifest_dir leaves the project root: {}".format(label, repr(value)))
            components.pop()
        else:
            components.append(component)
    return "/".join(components) if components else "."

def _bin_output(label: Label, name: str, dep: Dependency) -> cmd_args:
    outputs = dep[DefaultInfo].default_outputs
    if len(outputs) != 1:
        fail("{}: cargo_bin_exes[{}] must have exactly one default output".format(label, repr(name)))
    return cmd_args(outputs[0], hidden = [dep[RunInfo], dep[DefaultInfo].other_outputs])

def cargo_bin_exe_compile_env(ctx: AnalysisContext) -> dict[str, cmd_args]:
    """Compile-time `CARGO_BIN_EXE_<name>`: project-relative paths, not compile inputs."""
    env = {}
    for name, dep in getattr(ctx.attrs, "cargo_bin_exes", {}).items():
        _check_bin_name(ctx.label, name)
        outputs = dep[DefaultInfo].default_outputs
        if len(outputs) != 1:
            fail("{}: cargo_bin_exes[{}] must have exactly one default output".format(ctx.label, repr(name)))
        env[CARGO_BIN_EXE_PREFIX + name] = cmd_args(outputs[0], ignore_artifacts = True)
    if _runner_applies(ctx):
        env[CARGO_TARGET_TMPDIR] = cmd_args(CARGO_TARGET_TMPDIR_PATH)
    return env

def _runner_applies(ctx: AnalysisContext) -> bool:
    """Whether `ctx` is a libtest test that runs under the toolchain's per-test runner."""
    if not getattr(ctx.attrs, "framework", False):
        return False
    toolchain = getattr(ctx.attrs, "_rust_toolchain", None)
    return toolchain != None and toolchain[RustToolchainInfo].test_process_runner != None

def _source_resource_files(ctx: AnalysisContext) -> list[Artifact]:
    """Every file of `source_resource_sets`, refusing a set with a built output."""
    files = []
    for dep in ctx.attrs.source_resource_sets:
        for output in dep[DefaultInfo].default_outputs:
            if not output.is_source:
                fail("{}: source_resource_sets {} has the built output {}; only source artifacts lie at their project paths".format(ctx.label, dep.label, output))
            files.append(output)
    return files

def _source_inputs(ctx: AnalysisContext) -> cmd_args:
    """The target's source files and extra resources as hidden inputs of its test command."""
    return cmd_args(hidden = [src for src in ctx.attrs.srcs if src.is_source] + ctx.attrs.extra_resources + _source_resource_files(ctx))

def _runtime_tools_dir(ctx: AnalysisContext, nix_manifest: Artifact | None) -> cmd_args:
    """The test's PATH: its runtime tools directory, bound to the toolchain's manifest."""
    info = ctx.attrs.runtime_tools[RuntimeToolsInfo]
    if nix_manifest == None or info.nix_manifest != nix_manifest:
        fail("{}: runtime_tools {} is not bound to the rust toolchain's declared runtime manifest".format(ctx.label, ctx.attrs.runtime_tools.label))
    return cmd_args(info.directory, hidden = [info.nix_manifest])

def cargo_test_runtime(ctx: AnalysisContext, runner_applies: bool, remote: bool, nix_manifest: Artifact | None = None) -> (dict[str, typing.Any], list[typing.Any]):
    """The test's runtime entries and the runner options naming its path entries.

    `remote` says the test may run on a worker; the caller has already
    required the runner for such a test. Fails when another writer already
    declares one of the entries.
    """
    if remote and not runner_applies:
        fail("{}: a remote rust_test needs the libtest process runner".format(ctx.label))
    remote_paths = _REMOTE_PATHS if remote else []
    account_homes = _REMOTE_ACCOUNT_HOME if remote else []
    label = ctx.label
    for key in ctx.attrs.env:
        if key.startswith(CARGO_BIN_EXE_PREFIX):
            fail("{}: env must not set {}; use cargo_bin_exes".format(label, key))
    for field in ["env", "run_env"]:
        if CARGO_TARGET_TMPDIR in getattr(ctx.attrs, field):
            fail("{}: {} must not set {}; the prelude writes it".format(label, field, CARGO_TARGET_TMPDIR))
    bins = {}
    for name, dep in ctx.attrs.cargo_bin_exes.items():
        _check_bin_name(label, name)
        bins[CARGO_BIN_EXE_PREFIX + name] = _bin_output(label, name, dep)
    tool_paths = ["PATH"] if ctx.attrs.runtime_tools != None else []
    owned = _FIXED_PATHS + list(_FIXED_VALUES.keys()) + bins.keys() + remote_paths + account_homes + tool_paths
    for key in tool_paths:
        if key in ctx.attrs.env:
            fail("{}: env must not set PATH with runtime_tools; the prelude writes it".format(label))
    for key in remote_paths + account_homes:
        if key in ctx.attrs.env:
            fail("{}: env must not set {} on a remote test; the prelude writes it".format(label, key))
    for key in ctx.attrs.run_env:
        if key in owned or key.startswith(CARGO_BIN_EXE_PREFIX):
            fail("{}: run_env must not set {}; the prelude writes it".format(label, key))
    args = ctx.attrs.args
    for index, argument in enumerate(args):
        named = None
        if argument == PROJECT_RELATIVE_ENV and index + 1 < len(args):
            named = args[index + 1]
        elif argument.startswith(PROJECT_RELATIVE_ENV + "="):
            named = argument[len(PROJECT_RELATIVE_ENV + "="):]
        if named != None and (named in owned or named.startswith(CARGO_BIN_EXE_PREFIX)):
            fail("{}: args must not name {} in {}; the prelude writes it".format(label, named, PROJECT_RELATIVE_ENV))
        if argument == PROJECT_RELATIVE_DIR or argument.startswith(PROJECT_RELATIVE_DIR + "="):
            fail("{}: args must not pass {}; the prelude creates {}".format(label, PROJECT_RELATIVE_DIR, CARGO_TARGET_TMPDIR_PATH))

    if not runner_applies:
        # Only the runner puts the tools directory on PATH; without it the
        # declared tools would silently give way to the host's.
        if ctx.attrs.runtime_tools != None:
            fail("{}: runtime_tools needs the libtest process runner (framework = True and a toolchain test_process_runner); without it PATH would not be the declared tools".format(label))
        return {}, [_source_inputs(ctx)]

    manifest_dir = ctx.attrs.cargo_manifest_dir
    if manifest_dir == None:
        manifest_dir = ctx.label.package or "."
    env = {
        "CARGO_MANIFEST_DIR": _normalized_project_path(label, manifest_dir),
        "INSTA_WORKSPACE_ROOT": ".",
        "METACORTEX_WORKSPACE_ROOT": ".",
    } | _FIXED_VALUES | bins | {name: "." for name in remote_paths}
    if tool_paths:
        env["PATH"] = _runtime_tools_dir(ctx, nix_manifest)
    runner_args = [_source_inputs(ctx)]
    for name in sorted(_FIXED_PATHS + bins.keys() + remote_paths + tool_paths):
        runner_args.extend([PROJECT_RELATIVE_ENV, name])
    runner_args.extend([PROJECT_RELATIVE_DIR, CARGO_TARGET_TMPDIR_PATH])
    for name in account_homes:
        runner_args.extend([ACCOUNT_HOME_DIR_ENV, name])
    return env, runner_args
