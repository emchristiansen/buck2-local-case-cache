"""Local Linux/Clang toolchains backed by exact Nix store executable paths."""

load("@prelude//toolchains:cxx.bzl", "CxxToolsInfo")
load("@prelude//cxx:cxx_toolchain_types.bzl", "BinaryUtilitiesInfo", "CxxPlatformInfo", "CxxToolchainInfo", "LinkerInfo", "LinkerType")
load("@prelude//cxx:linker.bzl", "LINKERS")

def _pinned_cxx_tools_impl(ctx):
    return [
        DefaultInfo(),
        CxxToolsInfo(
            compiler = ctx.attrs.cc[RunInfo].args,
            cxx_compiler = ctx.attrs.cxx[RunInfo].args,
            asm_compiler = ctx.attrs.cc[RunInfo].args,
            compiler_type = "clang",
            asm_compiler_type = "clang",
            archiver = ctx.attrs.ar[RunInfo].args,
            archiver_type = "gnu",
            linker = ctx.attrs.cc[RunInfo].args,
            linker_type = LinkerType(ctx.attrs.linker_type),
        ),
    ]

pinned_cxx_tools = rule(
    impl = _pinned_cxx_tools_impl,
    attrs = {
        "linker_type": attrs.enum(LinkerType.values()),
    } | {
        name: attrs.exec_dep(providers = [RunInfo])
        for name in ["cc", "cxx", "ar"]
    },
)

def _pinned_cxx_toolchain_impl(ctx):
    base = ctx.attrs.template[CxxToolchainInfo]
    fields = {name: getattr(base, name) for name in [
        "internal_tools",
        "linker_info",
        "bolt_enabled",
        "cxx_compiler_info",
        "c_compiler_info",
        "as_compiler_info",
        "asm_compiler_info",
        "cvtres_compiler_info",
        "rc_compiler_info",
        "header_mode",
        "cpp_dep_tracking_mode",
        "pic_behavior",
        "llvm_link",
        "use_dep_files",
        "runtime_dependency_handling",
    ]}
    linker_fields = {name: getattr(base.linker_info, name) for name in [
        "archiver",
        "archiver_flags",
        "archiver_reads_inputs",
        "archiver_supports_argfiles",
        "archiver_type",
        "archive_contents",
        "archive_objects_locally",
        "archive_symbol_table",
        "binary_extension",
        "dist_thin_lto_codegen_flags",
        "extra_outputs",
        "generate_linker_maps",
        "generate_gc_sections",
        "link_binaries_locally",
        "link_libraries_locally",
        "link_style",
        "link_weight",
        "link_ordering",
        "linker",
        "linker_flags",
        "executable_linker_flags",
        "binary_linker_flags",
        "link_metadata_flag",
        "lto_mode",
        "mk_shlib_intf",
        "object_file_extension",
        "post_linker_flags",
        "runtime_library_files",
        "sanitizer_runtime_enabled",
        "sanitizer_runtime_files",
        "shlib_interfaces",
        "shared_dep_runtime_ld_flags",
        "shared_library_name_default_prefix",
        "shared_library_name_format",
        "shared_library_versioned_name_format",
        "static_dep_runtime_ld_flags",
        "static_library_extension",
        "static_pic_dep_runtime_ld_flags",
        "requires_archives",
        "requires_objects",
        "supports_distributed_thinlto",
        "independent_shlib_interface_linker_flags",
        "thin_lto_premerger_enabled",
        "thin_lto_double_codegen_enabled",
        "type",
        "use_archiver_flags",
        "force_full_hybrid_if_capable",
        "is_pdb_generated",
        "push_pop_state_flags",
        "supports_content_based_paths_for_archiving",
        "incremental_linking",
        "supports_shared_libraries",
    ]}
    if sorted(linker_fields) != sorted(dir(base.linker_info)):
        fail("Pinned LinkerInfo forwarding fields differ from the provider schema")
    # The template names shared libraries `{}.so` on every non-Windows OS;
    # derive the names from the linker type as the prelude's cxx_toolchain does.
    linker = LINKERS[base.linker_info.type]
    overrides = {
        "link_binaries_locally": False,
        "shared_library_name_format": "{}." + linker.default_shared_library_extension,
        "shared_library_versioned_name_format": "{}." + linker.default_shared_library_versioned_extension_format,
    }
    linker_fields.update(overrides)
    fields["linker_info"] = LinkerInfo(**linker_fields)
    for name in linker_fields:
        if name not in overrides and getattr(fields["linker_info"], name) != getattr(base.linker_info, name):
            fail("Pinned LinkerInfo changed an unrelated field: " + name)
    fields["binary_utilities_info"] = BinaryUtilitiesInfo(
        nm = ctx.attrs.nm[RunInfo],
        objcopy = ctx.attrs.objcopy[RunInfo],
        objdump = ctx.attrs.objdump[RunInfo],
        ranlib = ctx.attrs.ranlib[RunInfo],
        strip = ctx.attrs.strip[RunInfo],
    )
    fields["llvm_link"] = ctx.attrs.llvm_link[RunInfo]
    return [
        DefaultInfo(),
        CxxToolchainInfo(**fields),
        ctx.attrs.template[CxxPlatformInfo],
    ]

pinned_cxx_toolchain = rule(
    impl = _pinned_cxx_toolchain_impl,
    attrs = {
        "template": attrs.toolchain_dep(providers = [CxxToolchainInfo, CxxPlatformInfo]),
    } | {
        name: attrs.exec_dep(providers = [RunInfo])
        for name in ["nm", "objcopy", "objdump", "ranlib", "strip", "llvm_link"]
    },
    is_toolchain_rule = True,
)
