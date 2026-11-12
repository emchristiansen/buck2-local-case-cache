# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//utils:buckconfig.bzl", "read_bool")
# @lint-ignore-every FBCODEBZLADDLOADS

load("@prelude//utils:type_defs.bzl", "is_dict", "is_list", "is_select")
load("@shim//build_defs/lib:oss.bzl", "translate_target")
load("@shim//nix:rust_protobuf_generate.bzl", "rust_protobuf_generate")

prelude = native

CPP_UNITTEST_DEPS = [
    "shim//third-party/googletest:cpp_unittest_main",
]
CPP_FOLLY_UNITTEST_DEPS = [
    "gh_facebook_folly//folly/test/common:test_main_lib",
    "gh_facebook_folly//folly/ext/buck2:test_ext",
]

def _map_package_headers(headers):
    if headers == None or is_select(headers) or is_dict(headers):
        return headers
    subpackage_headers = {_subpackage_header_name(header): header for header in headers if _subpackage_header_name(header) != None}
    if subpackage_headers:
        return {(_subpackage_header_name(header) or header): header for header in headers}
    return headers

def _subpackage_header_name(header):
    package = native.package_name()
    prefix = "//" + package + "/"
    if not header.startswith(prefix):
        return None
    parts = header[2:].split(":", 1)
    if len(parts) != 2:
        return None
    label_package, label_name = parts
    return label_package[len(package) + 1 :] + "/" + label_name

def prebuilt_cpp_library(name, headers = None, linker_flags = None, private_linker_flags = None, **kwargs):
    prelude.prebuilt_cxx_library(name = name, exported_headers = headers, exported_linker_flags = linker_flags, linker_flags = private_linker_flags, **kwargs)

def cpp_library(
    name,
    deps = [],
    srcs = [],
    external_deps = [],
    exported_deps = [],
    exported_external_deps = [],
    undefined_symbols = None,
    visibility = ["PUBLIC"],
    modular_headers = None,
    labels = None,
    linker_flags = None,
    private_linker_flags = None,
    exported_linker_flags = None,
    headers = None,
    private_headers = None,
    propagated_pp_flags = (),
    feature = None,
    preferred_linkage = None,
    cpp_compiler_flags = None,
    **kwargs,
):
    base_path = native.package_name()
    oss_depends_on_folly = read_bool("oss_depends_on", "folly", False)
    header_base_path = base_path
    if oss_depends_on_folly and header_base_path.startswith("folly"):
        header_base_path = header_base_path.replace("folly/", "", 1)
    if "header_namespace" in kwargs:
        header_base_path = kwargs["header_namespace"]
        kwargs = {key: value for key, value in kwargs.items() if key != "header_namespace"}

    _unused = (undefined_symbols, modular_headers, labels, propagated_pp_flags, feature, preferred_linkage)  # @unused
    if cpp_compiler_flags != None:
        if "compiler_flags" in kwargs:
            kwargs["compiler_flags"] = kwargs["compiler_flags"] + cpp_compiler_flags
        else:
            kwargs["compiler_flags"] = cpp_compiler_flags
    if headers == None:
        headers = []
    if labels != None and "oss_dependency" in labels:
        if oss_depends_on_folly:
            headers = [item.replace("//:", "//folly:") if item == "//:folly-config.h" else item for item in headers]
    linker_flags = _combine_deps(linker_flags, exported_linker_flags)
    headers = _map_package_headers(headers)
    private_headers = _map_package_headers(private_headers)
    prelude.cxx_library(
        name = name,
        srcs = srcs,
        deps = _fix_deps(_combine_deps(deps, external_deps_to_targets(external_deps))),
        exported_deps = _fix_deps(_combine_deps(exported_deps, external_deps_to_targets(exported_external_deps))),
        visibility = visibility,
        preferred_linkage = "static",
        exported_headers = headers,
        headers = private_headers,
        exported_linker_flags = linker_flags,
        linker_flags = private_linker_flags,
        header_namespace = header_base_path,
        **kwargs,
    )

def cpp_unittest(
    name,
    deps = [],
    external_deps = [],
    visibility = ["PUBLIC"],
    supports_static_listing = None,
    allocator = None,
    owner = None,
    labels = None,
    emails = None,
    extract_helper_lib = None,
    compiler_specific_flags = None,
    default_strip_mode = None,
    resources = {},
    test_main = None,
    versions = None,
    **kwargs,
):
    _unused = (supports_static_listing, allocator, owner, labels, emails, extract_helper_lib, compiler_specific_flags, default_strip_mode, versions)  # @unused
    if test_main != None:
        deps = deps + [test_main]
    elif read_bool("oss", "folly_cxx_tests", True):
        deps = deps + CPP_FOLLY_UNITTEST_DEPS
    else:
        deps = deps + CPP_UNITTEST_DEPS

    prelude.cxx_test(
        name = name, deps = _fix_deps(deps + external_deps_to_targets(external_deps)), visibility = visibility, resources = _fix_resources(resources), **kwargs
    )

def cpp_binary(
    name,
    deps = [],
    external_deps = [],
    visibility = ["PUBLIC"],
    dlopen_enabled = None,
    compiler_specific_flags = None,
    allocator = None,
    modules = None,
    **kwargs,
):
    _unused = (dlopen_enabled, compiler_specific_flags, allocator, modules)  # @unused
    prelude.cxx_binary(name = name, deps = _fix_deps(deps + external_deps_to_targets(external_deps)), visibility = visibility, **kwargs)

def java_binary(name, jar_style = None, runtime = None, *args, **kwargs):
    _unused = (jar_style, runtime)  # @unused
    return prelude.java_binary(name = name, *args, **kwargs)

# Attributes the fbcode rust macros accept and no prelude rust rule does: knobs
# of the Meta build (autocargo, build-info stamping, allocators, ...) and the
# ones the fbcode macros expand into further targets (`test_*`, `cxx_bridge`).
# To regenerate: call each prelude rust rule with every parameter of the fbcode
# `rust_library`, `rust_binary` and `rust_unittest` macros and collect the
# "extra named parameter" errors. `external_deps` is deliberately not here:
# dropping it would hide a missing dependency behind a compile error.
_FBCODE_ONLY_RUST_ATTRS = (
    "allocator",
    "allow_jni_merging",
    "allow_oss_build",
    "autocargo",
    "bolt_args_map",
    "cpp_deps",
    "cxx_bridge",
    "cxx_bridge_compiler_flags",
    "cxx_bridge_header",
    "cxx_bridge_header_namespace",
    "default_strip_mode",
    "fbcode_no_silent_dwp_overflow_override",
    "fbcode_skip_header_map",
    "fbconfig_rule_type",
    "keep_gpu_sections",
    "late_build_info_stamping",
    "miri",
    "nodefaultlibs",
    "part_of_sysroot",
    "skip_autocargo_manifest",
    "strip_mode",
    "test_compatible_with",
    "test_contacts",
    "test_deps",
    "test_env",
    "test_external_deps",
    "test_features",
    "test_incoming_transition",
    "test_keep_gpu_sections",
    "test_labels",
    "test_link_group_map",
    "test_link_style",
    "test_linker_flags",
    "test_mapped_srcs",
    "test_miri_flags",
    "test_named_deps",
    "test_network_access",
    "test_remote_execution",
    "test_resources",
    "test_run_env",
    "test_rustc_flags",
    "test_srcs",
    "unittests",
    "unittests_miri",
    "unstable_plugin",
    "versions",
)

def _drop_fbcode_only_rust_attrs(kwargs):
    return {k: v for k, v in kwargs.items() if k not in _FBCODE_ONLY_RUST_ATTRS}

def rust_library(
    name,
    edition = None,
    rustc_flags = [],
    deps = [],
    named_deps = None,
    mapped_srcs = {},
    resources = {},
    visibility = ["PUBLIC"],
    **kwargs,
):
    _unused = (named_deps, visibility)  # @unused
    deps = _fix_deps(deps)
    mapped_srcs = _maybe_select_map(mapped_srcs, _fix_mapped_srcs)
    resources = _maybe_select_map(resources, _fix_resources)

    # Reset visibility because internal and external paths are different.
    visibility = ["PUBLIC"]

    prelude.rust_library(
        name = name,
        edition = edition or _default_rust_edition(),
        rustc_flags = rustc_flags + [_CFG_BUCK_BUILD],
        deps = deps,
        visibility = visibility,
        mapped_srcs = mapped_srcs,
        resources = resources,
        **_drop_fbcode_only_rust_attrs(kwargs),
    )

    _rust_library_unittest(
        name = name,
        edition = edition,
        rustc_flags = rustc_flags,
        deps = deps,
        mapped_srcs = mapped_srcs,
        resources = resources,
        kwargs = kwargs,
    )

# The remote test profile the `<name>-unittest` targets name. Profiles are
# declared on toolchains//:remote_test_execution (shim/BUCK);
# `nativelink_test_result_reuse` requires the worker's build properties plus
# `muninn-test-runtime=linux-unprivileged-v2` and forbids local execution.
_RUST_UNITTEST_REMOTE_PROFILE = read_config("shim", "rust_unittest_remote_profile", "nativelink_test_result_reuse")

# The prelude's remote per-case route (rust/rust_binary.bzl rust_test_impl,
# `remote_case_result_reuse`): besides the External provider, each harness offers the
# Internal one, whose engine lists the cases and runs each as its own remote action,
# `runner -- BINARY <runner options> --raw-one-case --exact NAME`, reusing a cached result
# only when the strict parser confirms it as one PASS for that case. The prelude admits it
# only on `nativelink_test_result_reuse`, so another profile turns it off.
_RUST_UNITTEST_REMOTE_CASE_RESULT_REUSE = _RUST_UNITTEST_REMOTE_PROFILE == "nativelink_test_result_reuse" and read_bool(
    "shim",
    "rust_unittest_remote_case_result_reuse",
    True,
)

_RUST_UNITTEST_RUN_ENV = {"HOME": ".", "LC_ALL": "C", "TMPDIR": "."}
_RUST_UNITTEST_ARGS = ["--project-relative-env", "HOME", "--project-relative-env", "TMPDIR"]

# rust_library attributes that also describe how the crate's own unit-test
# harness is compiled. Everything else (doctests, doc_deps, preferred_linkage,
# proc_macro, ...) is library-only.
_RUST_UNITTEST_SHARED_ATTRS = (
    "srcs",
    "crate",
    "crate_root",
    "features",
    "env",
    "labels",
    "link_style",
    "target_compatible_with",
    "compatible_with",
)

def _rust_library_unittest(name, edition, rustc_flags, deps, mapped_srcs, resources, kwargs):
    """Emit `<name>-unittest`, the crate's libtest harness, as fbcode's rust_library does.

    The fbcode macro builds a `rust_test` from the library's own sources plus
    `test_deps`/`test_env`/`test_srcs`/`test_rustc_flags`; upstream's OSS shim
    dropped all of those, so no engine unit test had a Buck label. Here the
    harness is emitted when the library declares its tests (`test_deps`,
    `test_env`, `test_srcs` or `unittests = True`) and has not opted out with
    `unittests = False`. Proc-macro crates are skipped (their harness is not a
    proc-macro crate). The label is `<name>-unittest`, fbcode's spelling.
    """
    unittests = kwargs.get("unittests", None)
    if unittests == False or kwargs.get("proc_macro", False):
        return
    declares_tests = unittests == True or any([kwargs.get(attr) != None for attr in ("test_deps", "test_env", "test_srcs")])
    if not declares_tests:
        return

    test_kwargs = {k: kwargs[k] for k in _RUST_UNITTEST_SHARED_ATTRS if k in kwargs}
    test_kwargs.setdefault("crate", name)
    if kwargs.get("test_srcs"):
        test_kwargs["srcs"] = test_kwargs.get("srcs", []) + kwargs["test_srcs"]
    test_env = kwargs.get("test_env")
    if test_env:
        test_kwargs["env"] = _merge_env(test_kwargs.get("env", {}), test_env)
    if "env" in test_kwargs:
        test_kwargs["env"] = _fix_env(test_kwargs["env"])
    if kwargs.get("test_labels"):
        test_kwargs["labels"] = test_kwargs.get("labels", []) + kwargs["test_labels"]

    prelude.rust_test(
        name = name + "-unittest",
        edition = edition or _default_rust_edition(),
        rustc_flags = rustc_flags + (kwargs.get("test_rustc_flags") or []) + [_CFG_BUCK_BUILD],
        deps = deps + _fix_deps(kwargs.get("test_deps") or []),
        mapped_srcs = mapped_srcs,
        resources = resources,
        remote_execution = _RUST_UNITTEST_REMOTE_PROFILE,
        # Muninn's portable rust_test contract (prelude rust/rust_binary.bzl
        # _rust_test_re_executors), as Muninn's generated targets declare it:
        # explicit test execution caching, the libtest framework, and HOME and
        # TMPDIR made project-relative on the worker before any test argument.
        supports_test_execution_caching = True,
        remote_case_result_reuse = _RUST_UNITTEST_REMOTE_CASE_RESULT_REUSE,
        run_env = _RUST_UNITTEST_RUN_ENV,
        args = _RUST_UNITTEST_ARGS,
        visibility = ["PUBLIC"],
        **test_kwargs
    )

def _merge_env(env, extra):
    # env and test_env may each be a select (e.g. gazebo/hmap's test_env).
    if is_select(extra):
        return select_map(extra, lambda e: _merge_env(env, e))
    if is_select(env):
        return select_map(env, lambda e: _merge_env(e, extra))
    return dict(env) | extra

def rust_binary(name, edition = None, rustc_flags = [], deps = [], resources = {}, visibility = ["PUBLIC"], **kwargs):
    deps = _fix_deps(deps)
    resources = _maybe_select_map(resources, _fix_resources)

    # @lint-ignore BUCKLINT: avoid "Direct usage of native rules is not allowed."
    prelude.rust_binary(
        name = name,
        edition = edition or _default_rust_edition(),
        rustc_flags = rustc_flags + [_CFG_BUCK_BUILD],
        deps = deps,
        resources = resources,
        visibility = visibility,
        **_drop_fbcode_only_rust_attrs(kwargs),
    )

def rust_unittest(name, edition = None, rustc_flags = [], deps = [], resources = {}, visibility = ["PUBLIC"], **kwargs):
    deps = _fix_deps(deps)
    resources = _maybe_select_map(resources, _fix_resources)

    prelude.rust_test(
        name = name,
        edition = edition or _default_rust_edition(),
        rustc_flags = rustc_flags + [_CFG_BUCK_BUILD],
        deps = deps,
        resources = resources,
        visibility = visibility,
        **_drop_fbcode_only_rust_attrs(kwargs),
    )

def rust_protobuf_library(
    name,
    srcs,
    build_script,
    protos = None,  # Pass a list of files. They'll be placed in the cwd. Prefer using proto_srcs.
    deps = None,
    test_deps = None,
    doctests = True,
    build_env = None,
    proto_srcs = None,
    crate_name = None,
):  # Use a proto_srcs() target, path is exposed as BUCK_PROTO_SRCS.
    _rust_protobuf_library(
        name,
        srcs,
        build_script,
        "buck2_protoc_dev",
        "0.14",
        protos,
        [
            "fbsource//third-party/rust:tonic",
            "fbsource//third-party/rust:tonic-prost",
        ]
        + (deps or []),
        test_deps,
        doctests,
        build_env,
        proto_srcs,
        crate_name,
    )

    native.alias(
        name = name,
        actual = ":" + name + "_prost",
        visibility = ["PUBLIC"],
    )

def _rust_protobuf_library(
    name,
    srcs,
    build_script,
    buck2_protoc_dev,
    prost_version,
    protos,  # Pass a list of files. They'll be placed in the cwd. Prefer using proto_srcs.
    deps,
    test_deps,
    doctests,
    build_env,
    proto_srcs,
    crate_name,
):  # Use a proto_srcs() target, path is exposed as BUCK_PROTO_SRCS.
    versioned_prost_target = {
        "0.14": "prost",
    }[prost_version]
    build_name = name + "-build-" + versioned_prost_target
    proto_name = name + "-proto-" + versioned_prost_target

    rust_binary(
        name = build_name,
        srcs = [build_script],
        crate_root = build_script,
        deps = [
            "//buck2/app/buck2_protoc_dev:" + buck2_protoc_dev,
        ],
    )

    build_env = build_env or {}
    build_env.update({
        "PROTOC": "$(exe shim//third-party/proto:protoc)",
        "PROTOC_INCLUDE": "$(location shim//third-party/proto:google_protobuf)",
    })
    if proto_srcs:
        build_env["BUCK_PROTO_SRCS"] = "$(location {})".format(proto_srcs)

    # Not prelude.genrule: its script runs under the undeclared host
    # `/usr/bin/env bash`. This runs the build script through the pinned bash
    # with nix-manifest.json hidden, with the genrule's cwd, env, OUT and output.
    rust_protobuf_generate(
        name = proto_name,
        srcs = (protos or [])
        + [
            "shim//third-party/proto:google_protobuf",
        ],
        build_script = ":" + build_name,
        env = build_env,
        nix_manifest = "shim//:nix-manifest.json",
    )

    new_deps = [
        {
            "0.14": "fbsource//third-party/rust:prost",
        }[prost_version]
    ] + (deps or [])

    rust_library(
        name = name + "_" + versioned_prost_target,
        crate = crate_name or name,
        srcs = srcs,
        doctests = doctests,
        env = {
            # This is where prost looks for generated .rs files
            "OUT_DIR": "$(location :{})".format(proto_name),
        },
        named_deps = {
            "generated_prost_target": ":{}".format(proto_name),
        },
        deps = new_deps,
        test_deps = test_deps,
        rustc_flags = ["-Aunused-crate-dependencies"],
    )

ProtoSrcsInfo = provider(fields = ["srcs"])

def _proto_srcs_impl(ctx):
    srcs = {src.basename: src for src in ctx.attrs.srcs}
    for dep in ctx.attrs.deps:
        for src in dep[ProtoSrcsInfo].srcs:
            if src.basename in srcs:
                fail("Duplicate src:", src.basename)
            srcs[src.basename] = src
    out = ctx.actions.copied_dir(ctx.attrs.name, srcs, has_content_based_path = False)
    return [DefaultInfo(default_output = out), ProtoSrcsInfo(srcs = srcs.values())]

proto_srcs = rule(
    impl = _proto_srcs_impl,
    attrs = {
        "deps": attrs.list(attrs.dep(), default = []),
        "srcs": attrs.list(attrs.source(), default = []),
    },
)

def ocaml_binary(name, deps = [], visibility = ["PUBLIC"], **kwargs):
    deps = _fix_deps(deps)

    prelude.ocaml_binary(name = name, deps = deps, visibility = visibility, **kwargs)

_CFG_BUCK_BUILD = "--cfg=buck_build"

def _maybe_select_map(v, mapper):
    if is_select(v):
        return select_map(v, mapper)
    return mapper(v)

def _fix_mapped_srcs(xs: dict[str, str]):
    # For reasons, this is source -> file path, which is the opposite of what
    # it should be.
    return {translate_target(k): v for (k, v) in xs.items()}

def _fix_deps(xs):
    if is_select(xs):
        return select_map(xs, lambda child_targets: _fix_deps(child_targets))
    return map(translate_target, xs)

# Macros whose one argument is a target label (Buck2 coerce/attr_type/arg.rs). Each keeps
# its own kind: `location` (target configuration), `location_exec` (execution
# configuration), `exe` (RunInfo, execution configuration) and `exe_target` (RunInfo,
# target configuration); only the label inside is translated.
_ENV_TARGET_MACROS = ("location", "location_exec", "exe", "exe_target")

# Internal tool labels that `translate_target` cannot map, because the OSS project has no
# such package, mapped to the declared pinned tool that stands in for them.
_ENV_MACRO_TARGETS = {
    # fbcode's LLVM 21 clang binary (gazebo/hmap HMAP_TEST_CLANG): the pinned toolchain's
    # clang (nix/pinned_tools.bzl "clang"), as a file artifact (shim BUCK :clang).
    "fbcode//buck2/platform/cxx/fbcode:llvm-21-bin/clang": "toolchains//:clang",
}

def _fix_env_target(target):
    return _ENV_MACRO_TARGETS.get(target) or translate_target(target)

def _fix_env_value(value):
    """Translate the target label of every `$(KIND LABEL)` in `value`, KIND in _ENV_TARGET_MACROS.

    The same translation `_fix_deps` applies to dependencies, so an environment macro
    names the same OSS producer a dependency on that label would. Other text, other
    macros and their spacing are kept unchanged.
    """
    if type(value) != type(""):
        return value
    parts = []
    rest = value
    for _ in range(len(value)):
        start = rest.find("$(")
        if start < 0:
            break
        end = rest.find(")", start)
        if end < 0:
            break
        body = rest[start + 2:end]
        words = body.split(" ")
        if len(words) == 2 and words[0] in _ENV_TARGET_MACROS and words[1]:
            body = words[0] + " " + _fix_env_target(words[1])
        parts.append(rest[:start] + "$(" + body + ")")
        rest = rest[end + 1:]
    parts.append(rest)
    return "".join(parts)

def _fix_env(env):
    # env may be a select (e.g. gazebo/hmap's test_env); every branch is translated.
    if is_select(env):
        return select_map(env, _fix_env)
    return {k: _fix_env_value(v) for k, v in env.items()}

def _fix_resources(resources):
    if is_list(resources):
        return [translate_target(r) for r in resources]

    if is_dict(resources):
        return {k: translate_target(v) for k, v in resources.items()}

    fail("Unexpected type {} for resources".format(type(resources)))

def _default_rust_edition():
    package = native.package_name()

    # Parse buckconfig entries in the following form:
    #
    #     [rust]
    #     default_edition = 2024
    #     default_edition:buck2 = 2021
    #     default_edition:buck2/dice = 2024
    #
    if package:
        split = package.split("/")
        for i in range(len(split)):
            parent_directory = "/".join(split[: len(split) - i])
            edition = read_config("rust", "default_edition:" + parent_directory)
            if edition != None:
                return edition

    return read_config("rust", "default_edition")

def thrift_library(name, thrift_srcs, languages, deps = [], py_base_module = None, rust_deps = [], thrift_rust_options = [], **kwargs):
    for l in languages:
        if False:
            pass
        else:
            print("FIXME(buck2-shims-meta): unsupported thrift language: {}".format(l))

# Do a nasty conversion of e.g. ("supercaml", None, "ocaml-dev") to
# 'fbcode//third-party-buck/platform010/build/supercaml:ocaml-dev'
# (which will then get mapped to `shim//third-party/ocaml:ocaml-dev`).
def external_dep_to_target(t):
    if type(t) == type(()):
        return "fbcode//third-party-buck/platform010/build/{}:{}".format(t[0], t[2])
    else:
        return "fbcode//third-party-buck/platform010/build/{}:{}".format(t, t)

def external_deps_to_targets(ts):
    if ts == None:
        return []
    if is_select(ts):
        return select_map(ts, external_deps_to_targets)
    return [external_dep_to_target(t) for t in ts]

def _combine_deps(left, right):
    if left == None:
        left = []
    if right == None:
        right = []
    return left + right
