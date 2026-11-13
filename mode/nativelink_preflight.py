#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

"""The workstation entry for the explicit NativeLink mode: check everything, then launch.

Inside Buck, an omitted mode is the upstream-compatible default (shim/build_defs/nativelink_mode.bzl),
which public CI needs. The workstation route must never fall into it, so this entry checks the
whole invocation and its pinned inputs before anything starts, and launches only if every check
passes. On any failure it prints every problem and exits 2 without launching.

    python3 mode/nativelink_preflight.py \
        --engine ENGINE --engine-sha256 HEX --launcher-sha256 HEX \
        --prelude-digest HEX --manifest-sha256 HEX --nix-cell-digest HEX --config-sha256 HEX \
        [--launcher-args N] [--check-only] \
        -- LAUNCHER... BUCK2_ARGS...

Run it from the project root. The first N words after `--` are the launcher (default 1: the
engine itself, or a wrapper such as buck2_funnel with its own options), and the rest are the buck2
arguments. The checks:
- the buck2 arguments are a `build` or `test` with `--isolation-dir`, `--remote-only`, exactly one
  `--config-file mode/nativelink.buckconfig` and no other config: no other `--config-file`, no
  `-c`/`--config` except `test.use_internal_runner=true` (required for `test`), no indirect
  argument file (`@FILE`, `--flagfile FILE`, which buck2 expands before parsing), no
  validate.buckconfig, no local or preferring execution, no target-platform or modifier override;
- mode/nativelink.buckconfig holds every setting of the mode and no startup setting, and the startup
  .buckconfig ignores `prelude` (read only when the daemon starts, so no flag can repair it later);
- the root .buckconfig.local is a regular file with exactly the bytes of
  mode/nativelink-startup.buckconfig, and the startup configuration buck2 reads without any
  command-line configuration (.buckconfig.d/common.buckconfig, .buckconfig, .buckconfig.local, as
  the daemon reads it to build its remote execution client) holds every startup setting;
- every daemon of the isolation read that startup configuration: a daemon keeps the remote
  execution client it started with, and buck2 records nothing of it. So the first launch of an
  isolation needs a new one (no daemon directory) and records the identity of the startup inputs
  (.buck2/nativelink-startup/<isolation>.json); a later launch in it, a TEST after its BUILD or a
  repeated TEST, needs that record with the same inputs, unchanged since, so that any daemon
  started in between read exactly them;
- .buck2/prelude-source resolves to a prelude whose tree digest (`--print-prelude-digest`) is the
  accepted one;
- shim/nix/nix-manifest.json has the accepted sha256 and every root it names exists;
- the environment the launcher inherits sets no BUCK* variable (buck2's configuration and
  test-injection variables, such as BUCK2_TEST_EXTRA_EXTERNAL_CONFIG);
- no configuration input buck2 would read beyond mode/nativelink.buckconfig, .buckconfig,
  .buckconfig.d/common.buckconfig and the root .buckconfig.local: no .buckconfig.local in another
  cell, no other .buckconfig.d file, no .buckconfig.d in another cell, no /etc or home buckconfig;
  the root cells are exactly the split's; the exact bytes of those four inputs and of
  mode/nativelink-startup.buckconfig have the accepted digest (`--print-config-digest`); the cells in
  effect after the mode applies are exactly the split's final map; and the nix_toolchains cell
  (shim/nix) has the accepted tree digest (`--print-prelude-digest shim/nix`);
- the engine file has the accepted sha256, and so does the launcher's executable; a launcher that
  is not the engine itself must name it in MUNINN_BUCK2_REAL.
The expected values come from the sealed accepted candidate, never from the tree being checked.
"""

import hashlib
import json
import os
import shutil
import stat
import sys
from pathlib import Path

MODE_FILE = "mode/nativelink.buckconfig"
# The startup settings' reviewed source, and the root project file a run copies it to.
STARTUP_TEMPLATE = "mode/nativelink-startup.buckconfig"
STARTUP_LOCAL = ".buckconfig.local"
ENDPOINT = "grpc://127.0.0.1:50051"
DETECTOR = (
    "target:root//...->shim//platforms:linux_x86_64 "
    "target:shim//...->shim//platforms:linux_x86_64"
)
# Every setting the mode file must hold, as mode/nativelink.buckconfig states it.
MODE_SETTINGS = {
    ("cells", "prelude"): ".buck2/prelude-source",
    ("cell_aliases", "toolchains"): "nix_toolchains",
    ("shim", "toolchain_mode"): "nativelink",
    ("build", "execution_platforms"): "shim//platforms:remote_linux",
    ("parser", "target_platform_detector_spec"): DETECTOR,
}
# Every setting the startup configuration must hold. buck2 reads these only from the root project
# configuration with no command-line configuration: the daemon builds its remote execution client
# from `BuckConfigBasedCells::parse_with_config_args(&fs, &[])` (buck2_server daemon/state.rs) and
# the client its startup configuration the same way (buck2_client_ctx immediate_config.rs).
STARTUP_SETTINGS = {
    ("buck2_re_client", "engine_address"): ENDPOINT,
    ("buck2_re_client", "action_cache_address"): ENDPOINT,
    ("buck2_re_client", "cas_address"): ENDPOINT,
    ("buck2_re_client", "tls"): "false",
    ("buck2", "digest_algorithms"): "SHA256",
}
# Sections read only at startup, so a setting of theirs in the mode file would have no effect.
STARTUP_SECTIONS = {"buck2_re_client", "buck2", "buck2_resource_control"}
# The root cell's project configuration: buck2 reads .buckconfig.d/*, then .buckconfig, then
# .buckconfig.local (app/buck2_common/src/legacy_configs/path.rs). The only accepted inputs are
# .buckconfig.d/common.buckconfig, .buckconfig and the run's .buckconfig.local.
ROOT_CONFIG_FOLDER = [".buckconfig.d/common.buckconfig"]
# The startup configuration's files, in buck2's reading order, and where the first launch of an
# isolation records their identity.
STARTUP_INPUTS = [".buckconfig.d/common.buckconfig", ".buckconfig", STARTUP_LOCAL]
STARTUP_RECORDS = ".buck2/nativelink-startup"
# buck2 reserves isolation names with this prefix (buck2_common invocation_paths.rs).
RESERVED_ISOLATION_PREFIX = "._buck2"
# The cells those two files declare, exactly; the mode then overrides `prelude` and adds
# `engine_prelude_source`.
ROOT_CELLS = {
    "gh_facebook_buck2": ".",
    "gh_facebook_buck2_shims_meta": "shim",
    "prelude": "prelude",
    "none": "none",
    "nix_toolchains": "shim/nix",
}
# The cells in effect once the mode file is applied after the startup configuration
# (legacy_configs/configs.rs): the mode replaces `prelude` and adds `engine_prelude_source`, and
# nothing else. Every cell the run uses is checked against this final map.
FINAL_CELLS = {
    **ROOT_CELLS,
    "prelude": ".buck2/prelude-source",
    "engine_prelude_source": "prelude",
}
# The accepted configuration inputs, in the order buck2 reads them, and the startup settings'
# reviewed source; `--config-sha256` binds their exact bytes (`--print-config-digest`).
CONFIG_INPUTS = [
    ".buckconfig.d/common.buckconfig",
    ".buckconfig",
    STARTUP_LOCAL,
    MODE_FILE,
    STARTUP_TEMPLATE,
]
# External configuration buck2 also reads (path.rs DEFAULT_EXTERNAL_CONFIG_SOURCES): none may exist.
EXTERNAL_CONFIG = [
    "/etc/buckconfig.d",
    "/etc/buckconfig",
    "~/.buckconfig.d",
    "~/.buckconfig.local",
]
# buck2 also takes configuration from its environment: BUCK2_TEST_EXTRA_EXTERNAL_CONFIG appends a
# config file (legacy_configs/cells.rs), and BUCK2_SETTINGS_OVERRIDE and BUCK2_TEST_SETTINGS_HOME_DIR
# redirect settings, read even in a production binary. Every variable buck2 reads that can carry
# configuration or test injection (the `buck2_env!` registry: all eleven `applicability = testing`
# entries, the settings and config ones) is named BUCK*; the registry's other names (CI,
# SANDCASTLE*, FORCE_WANT_RESTART, ...) are run metadata. So the whole BUCK* namespace is refused,
# whatever the variable; MUNINN_BUCK2_REAL, which names the engine, is outside it.
REFUSED_ENV_PREFIX = "BUCK"
INTERNAL_RUNNER = "test.use_internal_runner=true"
FORBIDDEN_FLAGS = {
    "--local-only",
    "--prefer-local",
    "--prefer-remote",
    "--target-platforms",
    "--modifier",
    "-m",
}
FLAGS_WITH_VALUES = {"--isolation-dir", "--config-file", "-c", "--config"}


class IncludeRefused(Exception):
    pass


def read_buckconfig(path: Path) -> dict[tuple[str, str], str]:
    """The `section.key = value` settings of a buckconfig file, continuation lines joined. An
    include line (`<file:…>`, `<?file:…>`) is refused rather than followed, so no setting can come
    from a file this does not read."""
    settings = {}
    section = None
    lines = path.read_text().splitlines()
    i = 0
    while i < len(lines):
        line = lines[i].strip()
        while line.endswith("\\") and i + 1 < len(lines):
            i += 1
            line = line[:-1].rstrip() + " " + lines[i].strip()
        i += 1
        if not line or line.startswith("#") or line.startswith(";"):
            continue
        if line.startswith("<"):
            raise IncludeRefused(f"{path.name} includes another file ({line}), which is refused")
        if line.startswith("[") and line.endswith("]"):
            section = line[1:-1].strip()
        elif "=" in line and section is not None:
            key, value = line.split("=", 1)
            settings[(section, key.strip())] = " ".join(value.split())
    return settings


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def prelude_digest(root: Path) -> str:
    """A digest of the tree at `root`: each entry's relative path, kind, executable bit and
    content (or link target), in sorted order. Links inside the tree are recorded, not followed."""
    lines = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for name in sorted(dirnames + filenames):
            path = Path(dirpath, name)
            rel = path.relative_to(root).as_posix()
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode):
                lines.append(f"link {rel} {os.readlink(path)}")
            elif stat.S_ISDIR(mode):
                lines.append(f"dir {rel}")
            elif stat.S_ISREG(mode):
                executable = "x" if mode & stat.S_IXUSR else "-"
                lines.append(f"file {rel} {executable} {sha256_file(path)}")
            else:
                lines.append(f"other {rel}")
    return hashlib.sha256("".join(line + "\n" for line in lines).encode()).hexdigest()


def config_digest(root: Path) -> str:
    """A digest of the accepted configuration inputs' exact bytes, in buck2's reading order."""
    lines = [f"{sha256_file(root / name)}  {name}\n" for name in CONFIG_INPUTS]
    return hashlib.sha256("".join(lines).encode()).hexdigest()


def argument_problems(args: list[str]) -> tuple[list[str], str | None]:
    """Why the buck2 arguments are not a complete NativeLink mode BUILD or TEST, and the isolation
    directory they name."""
    problems = []
    subcommand = None
    config_files = []
    configs = []
    isolation = None
    i = 0
    while i < len(args):
        arg = args[i]
        flag, has_inline, inline = arg.partition("=") if arg.startswith("--") else (arg, "", "")
        if arg.startswith("@") or flag == "--flagfile":
            # buck2 expands `@FILE` and `--flagfile FILE` into more arguments before it parses any,
            # so their contents would escape every check here; the entry takes its arguments
            # directly.
            problems.append(f"{arg!r}: indirect argument files (@FILE, --flagfile) are refused")
            if flag == "--flagfile" and not has_inline:
                i += 1
        elif flag in FLAGS_WITH_VALUES:
            if has_inline:
                value = inline
            elif i + 1 < len(args):
                i += 1
                value = args[i]
            else:
                problems.append(f"{flag} has no value")
                break
            if flag == "--isolation-dir":
                isolation = value
            elif flag == "--config-file":
                config_files.append(value)
            else:
                configs.append(value)
        elif arg.startswith("-c") and len(arg) > 2:
            # The attached short form, `-cSECTION.KEY=VALUE`.
            configs.append(arg[2:])
        elif flag in FORBIDDEN_FLAGS or (arg.startswith("-m") and len(arg) > 2):
            problems.append(f"{flag} is not allowed: the mode runs every action remote-only")
        elif subcommand is None and not arg.startswith("-"):
            subcommand = arg
        if "validate.buckconfig" in arg:
            problems.append(f"{arg!r}: validate.buckconfig is a closed-port static-only input")
        i += 1
    if subcommand not in ("build", "test"):
        problems.append(f"the buck2 subcommand is {subcommand!r}, not build or test")
    if not isolation:
        problems.append("--isolation-dir is missing")
    else:
        problems += isolation_problems(isolation)
    if "--remote-only" not in args:
        problems.append("--remote-only is missing")
    normalized = [os.path.normpath(f) for f in config_files]
    if normalized != [MODE_FILE]:
        problems.append(
            f"--config-file must be exactly one {MODE_FILE}, but it is {config_files!r}"
        )
    expected_configs = [INTERNAL_RUNNER] if subcommand == "test" else []
    if configs != expected_configs:
        problems.append(f"-c/--config must be exactly {expected_configs!r}, but it is {configs!r}")
    return problems, isolation


def isolation_problems(isolation: str) -> list[str]:
    """Why buck2 would refuse `isolation` as an isolation directory name, by its own rules: not
    reserved (buck2/src/lib.rs `parse_isolation_dir`), and a single file name, not `.`, `..` or
    anything with a slash or backslash (buck2_fs file_name.rs `verify_file_name`). The name is
    checked as given, never normalized, since it also names this entry's own record."""
    if isolation.startswith(RESERVED_ISOLATION_PREFIX):
        return [f"--isolation-dir {isolation!r} starts with {RESERVED_ISOLATION_PREFIX!r}, which buck2 reserves"]
    if isolation in (".", "..") or "/" in isolation or "\\" in isolation:
        return [f"--isolation-dir {isolation!r} is not a single directory name"]
    return []


def environment_problems(environ) -> list[str]:
    """Why the environment the launcher would inherit could configure buck2. Values are not
    shown, only names."""
    return [
        f"the environment sets {name} (value not shown): buck2 reads BUCK* variables, which can "
        "configure it outside the accepted inputs"
        for name in sorted(environ)
        if name.startswith(REFUSED_ENV_PREFIX)
    ]


def input_problems(root: Path) -> list[str]:
    """Why the mode file or the startup config does not hold the whole mode, or why some other
    configuration buck2 would read could change it."""
    problems = []
    mode_path = root / MODE_FILE
    if not mode_path.is_file():
        return [f"{MODE_FILE} is missing"]
    for source in EXTERNAL_CONFIG:
        if os.path.lexists(os.path.expanduser(source)):
            problems.append(f"{source} exists: buck2 would read it, and it is not an accepted input")
    folder = root / ".buckconfig.d"
    found = sorted(f".buckconfig.d/{p.name}" for p in folder.iterdir()) if folder.is_dir() else []
    if found != ROOT_CONFIG_FOLDER:
        problems.append(f".buckconfig.d must hold exactly {ROOT_CONFIG_FOLDER}, but it holds {found}")
    template = root / STARTUP_TEMPLATE
    local = root / STARTUP_LOCAL
    if not template.is_file():
        problems.append(f"{STARTUP_TEMPLATE} is missing")
    elif not os.path.lexists(local):
        problems.append(
            f"{STARTUP_LOCAL} is missing: the run's startup settings are a copy of {STARTUP_TEMPLATE}"
        )
    elif not stat.S_ISREG(local.lstat().st_mode):
        problems.append(f"{STARTUP_LOCAL} is not a regular file")
    elif local.read_bytes() != template.read_bytes():
        problems.append(f"{STARTUP_LOCAL} is not exactly {STARTUP_TEMPLATE}")
    if problems:
        return problems
    try:
        mode = read_buckconfig(mode_path)
        # The startup configuration, as the daemon reads it with no command-line configuration.
        startup = read_buckconfig(root / ".buckconfig.d/common.buckconfig")
        startup.update(read_buckconfig(root / ".buckconfig"))
        startup.update(read_buckconfig(local))
    except IncludeRefused as e:
        return [str(e)]
    cells = {key: value for (section, key), value in startup.items() if section == "cells"}
    if cells != ROOT_CELLS:
        problems.append(f"the root cells must be exactly {ROOT_CELLS}, but they are {cells}")
    cells.update({key: value for (section, key), value in mode.items() if section == "cells"})
    if cells != FINAL_CELLS:
        problems.append(f"the cells in effect must be exactly {FINAL_CELLS}, but they are {cells}")
    for name, path in sorted(cells.items()):
        if name == "gh_facebook_buck2":
            continue
        for local in (".buckconfig.local", ".buckconfig.d"):
            if os.path.lexists(root / path / local):
                problems.append(f"the cell {name} has {path}/{local}, which is not an accepted input")
    for (section, key), value in MODE_SETTINGS.items():
        found = mode.get((section, key))
        if found != value:
            problems.append(f"{MODE_FILE} needs {section}.{key} = {value}, but it is {found!r}")
    for section, key in sorted(mode):
        if section in STARTUP_SECTIONS:
            problems.append(
                f"{MODE_FILE} sets {section}.{key}, which buck2 reads only at startup, so it would "
                "have no effect"
            )
    for (section, key), value in STARTUP_SETTINGS.items():
        found = startup.get((section, key))
        if found != value:
            problems.append(
                f"the startup configuration needs {section}.{key} = {value}, but it is {found!r}"
            )
    ignored = [p.strip() for p in startup.get(("project", "ignore"), "").split(",")]
    if "prelude" not in ignored:
        problems.append(".buckconfig project.ignore does not list prelude")
    return problems


def main(argv: list[str]) -> int:
    if argv[:1] == ["--print-config-digest"] and len(argv) == 1:
        print(config_digest(Path.cwd()))
        return 0
    if argv[:1] == ["--print-prelude-digest"] and len(argv) == 2:
        print(prelude_digest(Path(argv[1]).resolve()))
        return 0
    if "--" not in argv:
        print("usage: see this file's docstring; `--` and the command are required", file=sys.stderr)
        return 2
    split = argv.index("--")
    options, command = argv[:split], argv[split + 1 :]
    wanted = {
        "--engine": None,
        "--engine-sha256": None,
        "--launcher-sha256": None,
        "--prelude-digest": None,
        "--manifest-sha256": None,
        "--nix-cell-digest": None,
        "--config-sha256": None,
        "--launcher-args": "1",
    }
    check_only = False
    problems = []
    i = 0
    while i < len(options):
        if options[i] == "--check-only":
            check_only = True
        elif options[i] in wanted and i + 1 < len(options):
            wanted[options[i]] = options[i + 1]
            i += 1
        else:
            problems.append(f"unknown or incomplete option {options[i]!r}")
        i += 1
    for name, value in wanted.items():
        if value is None:
            problems.append(f"{name} is required")
    if problems:
        return report(problems)

    root = Path.cwd()
    count = int(wanted["--launcher-args"])
    launcher, buck2_args = command[:count], command[count:]
    if len(launcher) != count or not launcher:
        return report([f"the command has fewer than {count} launcher words"])
    argument_issues, isolation = argument_problems(buck2_args)
    problems += argument_issues
    problems += environment_problems(os.environ)
    problems += input_problems(root)
    first_launch = None
    if isolation and not problems:
        first_launch, startup_problems = startup_qualification(root, isolation, wanted["--config-sha256"])
        problems += startup_problems

    prelude_link = root / ".buck2/prelude-source"
    if not prelude_link.is_dir():
        problems.append(".buck2/prelude-source does not resolve to a directory")
    else:
        found = prelude_digest(prelude_link.resolve())
        if found != wanted["--prelude-digest"]:
            problems.append(
                f".buck2/prelude-source ({prelude_link.resolve()}) has digest {found}, "
                f"not the accepted {wanted['--prelude-digest']}"
            )

    if all((root / name).is_file() for name in CONFIG_INPUTS):
        found = config_digest(root)
        if found != wanted["--config-sha256"]:
            problems.append(
                f"the configuration inputs {CONFIG_INPUTS} have digest {found}, not the accepted "
                f"{wanted['--config-sha256']}"
            )
    else:
        problems.append(f"the configuration inputs {CONFIG_INPUTS} are not all files")
    nix_cell = root / FINAL_CELLS["nix_toolchains"]
    if not nix_cell.is_dir():
        problems.append("the nix_toolchains cell shim/nix is not a directory")
    else:
        found = prelude_digest(nix_cell.resolve())
        if found != wanted["--nix-cell-digest"]:
            problems.append(
                f"the nix_toolchains cell shim/nix has digest {found}, not the accepted "
                f"{wanted['--nix-cell-digest']}"
            )

    manifest = root / "shim/nix/nix-manifest.json"
    if not manifest.is_file():
        problems.append("shim/nix/nix-manifest.json is missing")
    else:
        found = sha256_file(manifest)
        if found != wanted["--manifest-sha256"]:
            problems.append(f"shim/nix/nix-manifest.json has sha256 {found}, not the accepted one")
        else:
            for entry in json.loads(manifest.read_text())["roots"]:
                if not os.path.exists(entry["path"]):
                    problems.append(f"the manifest root {entry['path']} does not exist")

    engine = Path(wanted["--engine"])
    if not engine.is_file():
        problems.append(f"the engine {engine} is not a file")
    elif sha256_file(engine) != wanted["--engine-sha256"]:
        problems.append(f"the engine {engine} does not have the accepted sha256")
    executable = shutil.which(launcher[0])
    if executable is None:
        problems.append(f"the launcher {launcher[0]} is not an executable")
    else:
        executable = os.path.realpath(executable)
        if sha256_file(Path(executable)) != wanted["--launcher-sha256"]:
            problems.append(f"the launcher {executable} does not have the accepted sha256")
        if engine.is_file() and executable != os.path.realpath(engine):
            named = os.environ.get("MUNINN_BUCK2_REAL")
            if named is None or os.path.realpath(named) != os.path.realpath(engine):
                problems.append(
                    f"the launcher is not the engine, so MUNINN_BUCK2_REAL must name {engine}, "
                    f"but it is {named!r}"
                )
    if problems:
        return report(problems)
    print(
        "nativelink preflight: every check passed for "
        + json.dumps({"cwd": str(root), "command": command}),
        file=sys.stderr,
    )
    if check_only:
        return 0
    if first_launch is not None:
        record, identity = first_launch
        try:
            write_record(root, record, identity)
        except OSError as e:
            return report([f"the startup record {record} could not be written: {e}"])
    os.execvp(command[0], command)


def record_location_problems(root: Path, record: Path) -> list[str]:
    """Why the startup record's directories or file could lead outside the run's tree: each of
    .buck2 and .buck2/nativelink-startup must be a real directory if it exists, and the record a
    regular file if it exists, never a link."""
    problems = []
    for path in (root / ".buck2", root / STARTUP_RECORDS):
        if os.path.lexists(path) and not stat.S_ISDIR(path.lstat().st_mode):
            problems.append(f"{path} is not a directory (a link is refused)")
    if not problems and os.path.lexists(record) and not stat.S_ISREG(record.lstat().st_mode):
        problems.append(f"the startup record {record} is not a regular file (a link is refused)")
    return problems


def write_record(root: Path, record: Path, identity: dict) -> None:
    """Write the startup record inside the run's tree, following no link."""
    for path in (root / ".buck2", root / STARTUP_RECORDS):
        try:
            path.mkdir()
        except FileExistsError:
            pass
        if not stat.S_ISDIR(path.lstat().st_mode):
            raise OSError(f"{path} is not a directory")
    fd = os.open(record, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o644)
    with os.fdopen(fd, "w") as f:
        f.write(json.dumps(identity, indent=2, sort_keys=True) + "\n")


def startup_identity(root: Path, config_sha256: str) -> dict:
    """What identifies the startup inputs: the accepted configuration digest, and each file's
    device, inode, change time and size. A write or replacement changes the change time."""
    files = {}
    for name in STARTUP_INPUTS:
        st = (root / name).stat()
        files[name] = [st.st_dev, st.st_ino, st.st_ctime_ns, st.st_size]
    return {"config_sha256": config_sha256, "files": files}


def startup_qualification(root: Path, isolation: str, config_sha256: str):
    """Whether every daemon of `isolation` read the checked startup configuration. Returns the
    record to write before a first launch (or None) and the problems."""
    # buck2's daemon directory for this project and isolation (buck2_common invocation_paths.rs
    # `daemon_dir`): `$HOME/.buck/buckd/<project root>/<isolation>`.
    daemon_dir = Path.home() / ".buck/buckd" / root.relative_to(root.anchor) / isolation
    record = root / STARTUP_RECORDS / f"{isolation}.json"
    location = record_location_problems(root, record)
    if location:
        return None, location
    identity = startup_identity(root, config_sha256)
    if not os.path.lexists(daemon_dir):
        return (record, identity), []
    try:
        fd = os.open(record, os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(fd) as f:
            recorded = json.loads(f.read())
    except (OSError, ValueError):
        recorded = None
    if recorded != identity:
        return None, [
            f"the isolation {isolation!r} already has a daemon directory ({daemon_dir}) but no "
            f"record that its daemons read these startup inputs, unchanged since its first launch "
            f"({record}); a daemon keeps the startup configuration it began with, so use a new "
            "isolation"
        ]
    return None, []


def report(problems: list[str]) -> int:
    for problem in problems:
        print(f"nativelink preflight: {problem}", file=sys.stderr)
    print("nativelink preflight: refused; nothing was launched", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
