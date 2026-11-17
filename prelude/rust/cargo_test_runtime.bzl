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

A test that runs remotely also gets worker-local `HOME` and `TMPDIR`, both `.`
(paths), so neither external listing nor the runner's own listing nor any test
reads or writes a home or temporary directory outside its action.

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

At compile time, `CARGO_BIN_EXE_<name>` is the binary's project-relative path,
without making the binary a compile input, so `env!`/`option_env!` sites
compile. That value is not absolute, unlike Cargo's; tests must take the path
from the runtime variable.
"""

load("@prelude//utils:cmd_script.bzl", "exec_script")

PROJECT_RELATIVE_ENV = "--project-relative-env"
CARGO_BIN_EXE_PREFIX = "CARGO_BIN_EXE_"

_FIXED_PATHS = ["CARGO_MANIFEST_DIR", "INSTA_WORKSPACE_ROOT", "METACORTEX_WORKSPACE_ROOT"]
_FIXED_VALUES = {"INSTA_UPDATE": "no"}
_REMOTE_PATHS = ["HOME", "TMPDIR"]
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
        "runtime_tools": attrs.dict(
            key = attrs.string(),
            value = attrs.string(),
            sorted = True,
            default = {},
            doc = """
                Program name -> absolute path of a declared tool (e.g. a store
                path of the toolchain's runtime manifest). The test's PATH is
                then exactly a directory holding one executable per name, each
                running its tool: the programs it may look up by name, and no
                others. Without entries, the prelude sets no PATH.
            """,
        ),
        "extra_resources": attrs.list(
            attrs.source(),
            default = [],
            doc = """
                Files outside the target's package that the test reads at run
                time, as labels. Each is an input of the test action at its
                own path: a source file (e.g. an `export_file` with
                `mode = "reference"`) at its project path.
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
    return env

def _source_inputs(ctx: AnalysisContext) -> cmd_args:
    """The target's source files and extra resources as hidden inputs of its test command."""
    return cmd_args(hidden = [src for src in ctx.attrs.srcs if src.is_source] + ctx.attrs.extra_resources)

def _runtime_tools_dir(ctx: AnalysisContext) -> cmd_args:
    """The project-relative directory of the test's runtime tools, one executable per name."""
    wrappers = {}
    for name, path in ctx.attrs.runtime_tools.items():
        if not name or any([c not in _BIN_NAME_CHARS for c in name.elems()]):
            fail("{}: runtime_tools key {} is not a program name".format(ctx.label, repr(name)))
        if not path.startswith("/"):
            fail("{}: runtime_tools[{}] is not an absolute path: {}".format(ctx.label, repr(name), repr(path)))
        wrappers[name] = exec_script(ctx.actions, "__runtime_tool_scripts__/" + name, cmd_args(path))

    # One directory holds them all; each output may otherwise get a directory
    # of its own (content-based paths).
    return cmd_args(ctx.actions.copied_dir("__runtime_tools__", wrappers))

def cargo_test_runtime(ctx: AnalysisContext, runner_applies: bool, remote: bool) -> (dict[str, typing.Any], list[typing.Any]):
    """The test's runtime entries and the runner options naming its path entries.

    `remote` says the test may run on a worker; the caller has already
    required the runner for such a test. Fails when another writer already
    declares one of the entries.
    """
    if remote and not runner_applies:
        fail("{}: a remote rust_test needs the libtest process runner".format(ctx.label))
    remote_paths = _REMOTE_PATHS if remote else []
    label = ctx.label
    for key in ctx.attrs.env:
        if key.startswith(CARGO_BIN_EXE_PREFIX):
            fail("{}: env must not set {}; use cargo_bin_exes".format(label, key))
    bins = {}
    for name, dep in ctx.attrs.cargo_bin_exes.items():
        _check_bin_name(label, name)
        bins[CARGO_BIN_EXE_PREFIX + name] = _bin_output(label, name, dep)
    tool_paths = ["PATH"] if ctx.attrs.runtime_tools else []
    owned = _FIXED_PATHS + list(_FIXED_VALUES.keys()) + bins.keys() + remote_paths + tool_paths
    for key in tool_paths:
        if key in ctx.attrs.env:
            fail("{}: env must not set PATH with runtime_tools; the prelude writes it".format(label))
    for key in remote_paths:
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

    if not runner_applies:
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
        env["PATH"] = _runtime_tools_dir(ctx)
    runner_args = [_source_inputs(ctx)]
    for name in sorted(_FIXED_PATHS + bins.keys() + remote_paths + tool_paths):
        runner_args.extend([PROJECT_RELATIVE_ENV, name])
    return env, runner_args
