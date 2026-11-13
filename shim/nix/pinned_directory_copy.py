"""Copy named subpaths of a pinned store directory into Buck outputs, refusing any link."""

import argparse
import os
import shutil
import stat
import sys
from pathlib import Path, PurePosixPath

_KINDS = {"directory": stat.S_ISDIR, "file": stat.S_ISREG}


def pinned_directory_copy(root: Path, subpath: str, kind: str, output: Path) -> None:
    """Copy `root/subpath`, which must be a `kind`, to the absent `output`.

    `root` is an absolute directory. `subpath` is a normalized relative path whose
    every component, `root`'s included, is no symlink. The walk never follows a link:
    a directory is created, a regular file's bytes and executable bits are copied,
    and any symlink or special file below is refused, naming it.
    """
    relative = PurePosixPath(subpath)
    if (
        not root.is_absolute()
        or relative.is_absolute()
        or relative.as_posix() != subpath
        or {"", ".", ".."} & set(subpath.split("/"))
    ):
        raise ValueError(f"{subpath!r} is not a normalized subpath of an absolute root")
    for parent in (root, *(root / part for part in relative.parents if part.parts)):
        if not stat.S_ISDIR(os.lstat(parent).st_mode):
            raise ValueError(f"{parent} is not a directory")
    source = root / subpath
    if not _KINDS[kind](os.lstat(source).st_mode):
        raise ValueError(f"{source} is not a {kind}")
    output.parent.mkdir(parents=True, exist_ok=True)
    _copy(source, output)


def _copy(source: Path, output: Path) -> None:
    mode = os.lstat(source).st_mode
    if stat.S_ISDIR(mode):
        output.mkdir()
        for name in sorted(os.listdir(source)):
            _copy(source / name, output / name)
    elif stat.S_ISREG(mode):
        with (
            os.fdopen(os.open(source, os.O_RDONLY | os.O_NOFOLLOW), "rb") as reader,
            os.fdopen(os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL), "wb") as writer,
        ):
            shutil.copyfileobj(reader, writer)
            os.fchmod(writer.fileno(), 0o644 | (mode & 0o111))
    elif stat.S_ISLNK(mode):
        raise ValueError(f"refusing symlink {source} -> {os.readlink(source)}")
    else:
        raise ValueError(f"refusing special file {source}")


def _main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument(
        "--copy", nargs=3, action="append", required=True, metavar=("KIND", "SUBPATH", "OUTPUT")
    )
    args = parser.parse_args(argv)
    try:
        for kind, subpath, output in args.copy:
            if kind not in _KINDS:
                raise ValueError(f"unknown kind {kind!r}")
            pinned_directory_copy(args.root, subpath, kind, Path(output))
    except (OSError, ValueError) as error:
        print(f"pinned_directory_copy: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(_main(sys.argv[1:]))
