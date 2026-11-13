"""Exact Nix store tool paths for the engine build and its unit tests.

Mirrors Muninn's toolchains/pinned_tools.bzl (recv-v2/ws, sha256 150b05a3...):
every tool is named by exact store path, and nix-manifest.json, the recursive
closure of these roots with narHash (as pinned_tool_generator/closure_info.py
computes it), is a hidden input of every action that runs one, so a NativeLink
worker can check the closure before it runs the action.

The C/C++, binutils, shell, Python and archive paths are W's own pins. The Rust
compiler is NOT W's (W pins stable rust-mixed with nightly_features=False):
it is the nightly named by this tree's rust-toolchain.toml, `nightly-2026-08-15`,
realised from the binary cache as rust-default-1.99.0-nightly-2026-08-15
(rustc 1.99.0-nightly d453bdd8f 2026-08-14, LLVM 23.1.0).

protoc and the protobuf well-known-type .proto files (decl-d2) are nixpkgs
`protobuf_21` (21.12, libprotoc 3.21.12) from the same package set as W's
python3/grep/tar/unzip pins, nixpkgs-compiler 7fd4004d0511991fff3a0524856a9be09e28c5f0
(W flake.lock), realised from cache.nixos.org. Its protoc's ELF interpreter is
the store glibc loader, so it needs no /lib64/ld-linux-x86-64.so.2 on the worker,
unlike the prebuilt protoc 21.4 zip the upstream third-party/proto producers fetch.
"""

_RUST = "/nix/store/7r733cfr21w4mcakw1pvaww2clg4ij6y-rust-default-1.99.0-nightly-2026-08-15"
_CLANG = "/nix/store/4kd8nkc6y3kqsmyr1fxmi50xckxap9js-clang-wrapper-23.1.3"
_LLVM = "/nix/store/ksz62mr81bd00ghrblz3kjsr9jccbgb6-llvm-23.1.3"
_NATIVE_BINT = "/nix/store/n7k2gvnv4q3piz9wwcs8bl6716xixmnb-llvm-binutils-wrapper-23.1.3"
_BINT = "/nix/store/7skl3l3bc22jf5rh2m4v39ffh6x3ckz3-binutils-2.47"
_BINT_WRAPPER = "/nix/store/jfhc4q9crbkfqxjszqcks3y1wp8g39dv-binutils-wrapper-2.47"
_BASH = "/nix/store/k51fszyzgm06i0iaajqa8a5rwb32wbfr-bash-5.3p20"
_COREUTILS = "/nix/store/q0ivambh8k5c49j7f2b8fi6d6sx3jg77-coreutils-9.12"
_GREP = "/nix/store/3668q0d9zhc5n9l2pnyvpd44kz9bjsjm-gnugrep-3.12"
_PYTHON = "/nix/store/lcfvy379m5j0parmzhs1icf6g0prck1q-python3-3.15.0"
_GNUTAR = "/nix/store/f4id9s3nydjrzxvgqfvhphsbql6ll26k-gnutar-1.35"
_GZIP = "/nix/store/vbanhaizwmy77hyk52s34h9z02wh6yyk-gzip-1.15"
_UNZIP = "/nix/store/h2jrsr48ss7i2lq9bscyqjmj2y8pmk4a-unzip-6.0"
_BZIP2 = "/nix/store/gz6baw96bf48pxs93kc7rxmnr1ra7g4m-bzip2-1.0.8-bin"
_XZ = "/nix/store/f10qh0sgpx38n3azj08dgbbnlxspv26n-xz-5.8.4-bin"
_ZSTD = "/nix/store/yfqdq38iw1k9n0a0w1ncsxpl2fs5gy94-zstd-1.5.7-bin"
_PROTOBUF = "/nix/store/56s8lsw67asqwb8ak42b3gcn95ydy5ry-protobuf-21.12"
_PROTOBUF_PROTO = "/nix/store/x3h3c6isb32pr46g1nvjxivj3whyagg0-protobuf-21.12-proto"

# The store roots whose recursive closure nix-manifest.json records.
PINNED_ROOTS = [
    _RUST,
    _CLANG,
    _LLVM,
    _NATIVE_BINT,
    _BINT,
    _BINT_WRAPPER,
    _BASH,
    _COREUTILS,
    _GREP,
    _PYTHON,
    _GNUTAR,
    _GZIP,
    _UNZIP,
    _BZIP2,
    _XZ,
    _ZSTD,
    _PROTOBUF,
    _PROTOBUF_PROTO,
]

PINNED_TOOLS = {
    "ar": {"closure": "cxx", "path": _BINT + "/bin/ar"},
    "as": {"closure": "cxx", "path": _BINT + "/bin/as"},
    "bash": {"closure": "cxx", "path": _BASH + "/bin/bash"},
    "cargo": {"closure": "rust_nightly", "path": _RUST + "/bin/cargo"},
    "cc": {"closure": "cxx", "path": _CLANG + "/bin/cc"},
    # The same wrapper's clang, which hands `-cc1` straight to the compiler with no
    # wrapper flags (gazebo/hmap's HMAP_TEST_CLANG; shims.bzl _ENV_MACRO_TARGETS).
    "clang": {"closure": "cxx", "path": _CLANG + "/bin/clang"},
    "clippy_driver": {"closure": "rust_nightly", "path": _RUST + "/bin/clippy-driver"},
    "cxx": {"closure": "cxx", "path": _CLANG + "/bin/c++"},
    "ld": {"closure": "cxx", "path": _NATIVE_BINT + "/bin/ld.lld"},
    "llvm_link": {"closure": "cxx", "path": _LLVM + "/bin/llvm-link"},
    # Creates the prost build scripts' $OUT (nix/rust_protobuf_generate.bzl).
    "mkdir": {"closure": "cxx", "path": _COREUTILS + "/bin/mkdir"},
    "nm": {"closure": "cxx", "path": _BINT + "/bin/nm"},
    "objcopy": {"closure": "cxx", "path": _BINT + "/bin/objcopy"},
    "objdump": {"closure": "cxx", "path": _BINT + "/bin/objdump"},
    "protobuf_proto": {"closure": "protobuf", "path": _PROTOBUF_PROTO},
    # prost-build's PROTOC and PROTOC_INCLUDE (shim/third-party/proto). The
    # include root is the `proto` output: only the twelve well-known-type
    # .proto files, all regular files (the `out` output's include/ links to them).
    "protoc": {"closure": "protobuf", "path": _PROTOBUF + "/bin/protoc"},
    "python": {"closure": "python", "path": _PYTHON + "/bin/python3"},
    "ranlib": {"closure": "cxx", "path": _BINT + "/bin/ranlib"},
    # Rust links through the Nix cc-wrapper (its ld-wrapper adds the store
    # RUNPATHs a test binary needs on the worker). W's muninn-lld-clang wrapper
    # is not used: it accepts only the -B selectors of W's two Rust toolchains
    # and refuses this nightly's. rustc's self-contained rust-lld is disabled
    # with `-Clinker-features=-lld` in the toolchain (see rust.bzl).
    "rust_linker": {"closure": "cxx", "path": _CLANG + "/bin/cc"},
    "rustc": {"closure": "rust_nightly", "path": _RUST + "/bin/rustc"},
    "rustdoc": {"closure": "rust_nightly", "path": _RUST + "/bin/rustdoc"},
    "strip": {"closure": "cxx", "path": _BINT_WRAPPER + "/bin/strip"},
}

PINNED_NIX_PATH = ":".join([
    root + "/bin"
    for root in [
        _CLANG,
        _LLVM,
        _NATIVE_BINT,
        _BINT_WRAPPER,
        _BINT,
        _BASH,
        _COREUTILS,
        _GREP,
        _PYTHON,
        _RUST,
    ]
])

PINNED_CXX_LINK_FLAGS = ["-B" + _NATIVE_BINT + "/bin/", "-fuse-ld=lld"]

PINNED_ARCHIVE_NIX_PATH = ":".join([
    root + "/bin"
    for root in [
        _CLANG,
        _LLVM,
        _NATIVE_BINT,
        _BINT_WRAPPER,
        _BINT,
        _BASH,
        _COREUTILS,
        _GREP,
        _PYTHON,
        _RUST,
        _GNUTAR,
        _GZIP,
        _UNZIP,
        _BZIP2,
        _XZ,
        _ZSTD,
    ]
])

PINNED_PYTHON_EXTENSION_LINKER_FLAGS = ["-L" + _PYTHON + "/lib", "-ldl", "-lm"]
