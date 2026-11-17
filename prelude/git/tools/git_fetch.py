# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

import argparse
import json
import os
import subprocess
import sys
import time
from enum import Enum
from pathlib import Path
from typing import List, NamedTuple


MAX_RETRIES = 5


def run(
    cmd: List[str], check: bool, environment: dict[str, str], retries: int = MAX_RETRIES
) -> str:
    print(f"Running {cmd}", file=sys.stderr)
    try:
        proc = subprocess.run(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            encoding="utf-8",
            check=check,
            env=environment,
        )
    except OSError as ex:
        print(ex, file=sys.stderr)
        sys.exit(1)
    except subprocess.CalledProcessError as ex:
        print(ex.stderr, file=sys.stderr)
        if "The requested URL returned error: 429" in ex.stderr and retries > 0:
            time.sleep(2 ** (MAX_RETRIES - retries))
            return run(cmd, check, environment, retries - 1)
        sys.exit(ex.returncode)
    return proc.stdout


class ObjectFormat(str, Enum):
    Sha1 = "sha1"
    Sha256 = "sha256"


class Args(NamedTuple):
    git: str
    runtime_manifest: Path
    git_dir: Path
    work_tree: Path
    object_format: ObjectFormat
    repo: str
    rev: str
    update_submodules: bool


def arg_parse() -> Args:
    parser = argparse.ArgumentParser()
    parser.add_argument("--git", type=str, required=True)
    parser.add_argument("--runtime-manifest", type=Path, required=True)
    parser.add_argument("--git-dir", type=Path, required=True)
    parser.add_argument("--work-tree", type=Path, required=True)
    parser.add_argument("--object-format", type=ObjectFormat, required=False)
    parser.add_argument("--update-submodules", action="store_true", required=False)
    parser.add_argument("--repo", type=str, required=True)
    parser.add_argument("--rev", type=str, required=True)
    return Args(**vars(parser.parse_args()))


def runtime_environment(args: Args) -> dict[str, str]:
    runtime = json.loads(args.runtime_manifest.read_text())
    if set(runtime) != {"version", "git", "python", "environment", "store_paths"}:
        raise RuntimeError("git_fetch runtime manifest has an unsupported shape")
    if type(runtime["version"]) is not int or runtime["version"] != 1:
        raise RuntimeError("git_fetch runtime manifest has an unsupported version")
    if args.git != runtime["git"]:
        raise RuntimeError("git_fetch Git differs from its declared runtime")
    if Path(sys.executable).resolve(strict=True) != Path(runtime["python"]).resolve(strict=True):
        raise RuntimeError("git_fetch Python differs from its declared runtime")
    store_paths = runtime["store_paths"]
    if not store_paths or store_paths != sorted(set(store_paths)):
        raise RuntimeError("git_fetch runtime closure must be nonempty, sorted and unique")
    for path in store_paths:
        if not path.startswith("/nix/store/") or not Path(path).exists():
            raise RuntimeError(f"git_fetch declared worker runtime input is absent: {path}")
    for path in (runtime["git"], runtime["python"]):
        if not any(Path(path).is_relative_to(root) for root in store_paths):
            raise RuntimeError(f"git_fetch executable is outside its declared closure: {path}")
    environment = runtime["environment"]
    if not isinstance(environment, dict) or any(
        not isinstance(key, str) or not isinstance(value, str)
        for key, value in environment.items()
    ):
        raise RuntimeError("git_fetch runtime environment must contain string keys and values")
    fixed_environment = {
        "HOME": "/homeless-shelter",
        "LANG": "C",
        "LC_ALL": "C",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_SYSTEM": "/dev/null",
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_TERMINAL_PROMPT": "0",
    }
    if set(environment) != set(fixed_environment) | {
        "PATH", "GIT_EXEC_PATH", "GIT_SSL_CAINFO", "SSL_CERT_FILE", "GIT_SSH_COMMAND"
    } or any(environment[key] != value for key, value in fixed_environment.items()):
        raise RuntimeError("git_fetch requires the closed declared Git environment")
    paths = environment["PATH"].split(":")
    if any(
        not Path(path).is_absolute()
        or not any(Path(path).is_relative_to(root) for root in store_paths)
        for path in paths
    ):
        raise RuntimeError("git_fetch PATH escapes its declared runtime closure")
    if environment["GIT_EXEC_PATH"] != str(Path(args.git).parent.parent / "libexec/git-core"):
        raise RuntimeError("git_fetch Git helpers differ from the declared Git")
    for key in ("GIT_SSL_CAINFO", "SSL_CERT_FILE"):
        path = environment[key]
        if not any(Path(path).is_relative_to(root) for root in store_paths) or not Path(path).is_file():
            raise RuntimeError(f"git_fetch declared certificate input is absent: {path}")
    return environment


def git_configure(git: List[str], environment: dict[str, str]):
    # Override if someone has `[core] autocrlf = true` in their global gitconfig
    # on a Windows machine. That causes LF to be converted into CRLF when
    # checking out text files, which is not desired here. Unix and Windows
    # should produce identical directory as output.
    run([*git, "config", "core.autocrlf", "false"], check=True, environment=environment)


def main() -> None:
    args = arg_parse()

    environment = runtime_environment(args)
    args.work_tree.mkdir(exist_ok=True)

    git_bin = args.git
    git = [git_bin, f"--git-dir={args.git_dir}", f"--work-tree={args.work_tree}"]
    if args.object_format is None:
        object_format_args = []
    else:
        object_format_args = ["--object-format", args.object_format]

    run([*git, "init"] + object_format_args, check=True, environment=environment)
    git_configure(git, environment)
    run([*git, "remote", "remove", "origin"], check=False, environment=environment)
    run([*git, "remote", "add", "origin", args.repo], check=True, environment=environment)
    run([*git, "fetch", "--depth=1", "origin", args.rev], check=True, environment=environment)

    fetch_head = run([*git, "rev-parse", "FETCH_HEAD"], check=True, environment=environment)
    fetch_head = fetch_head.strip()
    if fetch_head != args.rev:
        raise RuntimeError(
            f"fetched the wrong commit: expected {args.rev}, fetched {fetch_head}"
        )

    run([*git, "checkout", "FETCH_HEAD"], check=True, environment=environment)

    if args.update_submodules:
        git_in_worktree = [
            git_bin,
            f"--git-dir={os.path.abspath(args.git_dir)}",
            # `git submodule` does not accept `--work-tree`, so run it from the worktree.
            "-C",
            str(args.work_tree),
        ]
        run(
            [
                *git_in_worktree,
                "submodule",
                "update",
                "--init",
                "--recursive",
                "--depth=1",
            ],
            check=True,
            environment=environment,
        )


if __name__ == "__main__":
    main()
