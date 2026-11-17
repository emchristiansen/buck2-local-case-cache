# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load(
    "@prelude//:artifact_tset.bzl",
    "project_artifacts",
)
load("@prelude//:content_based_dist.bzl", "make_content_based_dist")
load(
    "@prelude//:resources.bzl",
    "create_relocatable_resources_info",
    "create_resource_db",
    "gather_resources",
)
load("@prelude//cxx:cxx_bolt.bzl", "cxx_use_bolt")
load("@prelude//cxx:cxx_executable.bzl", "get_cxx_post_link_suffix")
load(
    "@prelude//cxx:cxx_library_utility.bzl",
    "cxx_attr_deps",
)
load("@prelude//cxx:cxx_link_utility.bzl", "executable_shared_lib_arguments")
load(
    "@prelude//cxx:cxx_toolchain_types.bzl",
    "PicBehavior",
    "RuntimeDependencyHandling",
)
load("@prelude//cxx:cxx_utility.bzl", "cxx_attrs_get_allow_cache_upload")
load(
    "@prelude//cxx:link_groups.bzl",
    "LINK_GROUP_MAPPINGS_FILENAME_SUFFIX",
    "LINK_GROUP_MAPPINGS_SUB_TARGET",
    "LINK_GROUP_MAP_DATABASE_SUB_TARGET",
    "LinkGroupContext",
    "build_shared_libs_for_symlink_tree",
    "get_link_group_map_json",
)
load("@prelude//cxx:linker.bzl", "DUMPBIN_SUB_TARGET", "PDB_SUB_TARGET", "get_dumpbin_providers", "get_pdb_providers")
load("@prelude//cxx:transformation_spec.bzl", "build_transformation_spec_context")
load(
    "@prelude//dist:dist_info.bzl",
    "DistInfo",
)
load("@prelude//linking:add_elf_sections.bzl", "get_elf_sections")
load("@prelude//linking:generated_build_info.bzl", "compile_generated_build_info", "generate_build_info")
load(
    "@prelude//linking:link_info.bzl",
    "LibOutputStyle",
    "LinkStrategy",
    "get_lib_output_style",
    "process_link_strategy_for_pic_behavior",
    "unpack_link_args",
)
load(
    "@prelude//linking:linkable_graph.bzl",
    "create_linkable_graph",
)
load(
    "@prelude//linking:shared_libraries.bzl",
    "merge_shared_libraries",
    "traverse_shared_library_info",
)
load("@prelude//linking:stamp_build_info.bzl", "cxx_stamp_build_info", "stamp_build_info")
load("@prelude//os_lookup:defs.bzl", "OsLookup")
load("@prelude//rust/rust-analyzer:provider.bzl", "rust_analyzer_provider")
load("@prelude//test:inject_test_run_info.bzl", "inject_test_run_info")
load(
    "@prelude//tests:re_utils.bzl",
    "get_re_executors_from_props",
)
load("@prelude//tests:remote_test_execution_toolchain.bzl", "RemoteTestExecutionToolchainInfo")
load(
    "@prelude//utils:build_graph_pattern.bzl",
    "new_build_graph_info",
)
load("@prelude//utils:utils.bzl", "flatten_dict")
load(
    ":build.bzl",
    "generate_rustdoc",
    "get_inherited_link_args",
    "rust_compile",
    "rust_compile_invalidation_inputs",
    "rust_link_binary",
)
load(
    ":build_params.bzl",
    "BuildParams",  # @unused Used as a type
    "Emit",
    "LinkageLang",
    "ProfileMode",  # @unused Used as a type
    "RuleType",
    "build_params",
)
load(
    ":context.bzl",
    "CompileContext",
    "compile_context",
    "output_filename",
)
load(
    ":link_info.bzl",
    "DEFAULT_STATIC_LINK_STRATEGY",
    "attr_simple_crate_for_filenames",
    "inherited_external_debug_info",
    "inherited_linkable_graphs",
    "inherited_rust_cxx_link_group_info",
    "inherited_shared_libs",
)
load(":named_deps.bzl", "write_named_deps_names")
load(":outputs.bzl", "RustcExtraOutputsInfo", "output_as_diag_subtargets")
load(":profile.bzl", "make_profile_providers")
load(":resources.bzl", "rust_attr_resources")

def _get_runtime_dependency_handling(ctx: AnalysisContext, compile_ctx: CompileContext) -> RuntimeDependencyHandling:
    """Read runtime_dependency_handling from attribute, falling back to CXX toolchain default."""
    if ctx.attrs.runtime_dependency_handling:
        return RuntimeDependencyHandling(ctx.attrs.runtime_dependency_handling)

    # Toolchains that don't support shared libraries (e.g. WASM, bare-metal)
    # cannot create symlink trees, so skip runtime dependency handling.
    if not compile_ctx.cxx_toolchain_info.linker_info.supports_shared_libraries:
        return RuntimeDependencyHandling("no_symlink")
    return compile_ctx.cxx_toolchain_info.runtime_dependency_handling

def _strategy_params(ctx: AnalysisContext, compile_ctx: CompileContext) -> dict[LinkStrategy, BuildParams]:
    target_os_type = ctx.attrs._target_os_type[OsLookup]
    linker_type = compile_ctx.cxx_toolchain_info.linker_info.type

    params = {}
    for link_strategy in LinkStrategy:
        params[link_strategy] = build_params(
            rule = RuleType("binary"),
            proc_macro = False,
            link_strategy = link_strategy,
            lib_output_style = None,
            lang = LinkageLang("rust"),
            linker_type = linker_type,
            target_os_type = target_os_type,
        )
    return params

def _create_content_based_dist(
    ctx: AnalysisContext,
    name: str,
    exe: Artifact,
    relocatable_resources_json: Artifact | None,
    relocatable_resources_contents: Artifact | None,
    shlib_tree: Artifact | list | None,
) -> (Artifact, Artifact):
    """
    Build the content-based `assembled_dir` bundle for a `rust_binary` that opts
    into `has_content_based_path` and has adjacent files: the relocatable resource
    DB (copied) + the content-based `__resources` dir (symlinked), and the shlib
    symlink-tree (symlinked). The exe and the copy/symlink contract are handled by
    `make_content_based_dist`. Returns `(bundle_dir, projected exe)`.
    """
    exe_rel = exe.short_path
    copies = {}
    symlinks = {}

    # Resources: reuse the relocatable DB + `__<name>__resources` symlink dir
    # from `create_relocatable_resources_info` -- which is CONTENT-BASED for this
    # binary (rust_binary threads `has_content_based_path` through). Copy the DB
    # (its bytes are captured in the bundle); symlink the content-based resource
    # dir. The dir being content-based is load-bearing: its path is immutable and
    # re-hashes when a content-based resource changes, so the bundle's symlink to
    # it is trample-safe -- unlike the config-based default, which would be
    # rewritten in place under a running bundle.
    if relocatable_resources_json != None:
        copies[exe_rel + ".resources.json"] = relocatable_resources_json
        symlinks[relocatable_resources_contents.short_path] = relocatable_resources_contents

    # Shared libraries: symlink the whole prebuilt shlib symlink-tree into the
    # bundle at the `$ORIGIN/<tree>`-relative offset the exe's RPATH already
    # encodes (`get_rpath_origin` => `$ORIGIN` for the gnu linker). Symlinking
    # the tree the linker already targets -- rather than reconstructing it
    # per-soname -- reuses the tree tool's exact layout, so binaries with
    # computed (non-string) sonames are handled too, and the loader follows the
    # symlinked dir like any RPATH entry.
    if isinstance(shlib_tree, list) and shlib_tree:
        # Windows produces a list of per-DLL symlink artifacts (no `$ORIGIN`
        # RPATH); co-locating them here is unimplemented. Fail loudly rather
        # than ship a bundle that silently can't find its DLLs at runtime.
        fail(
            "content-based `assembled_dir` bundle does not support a Windows shared-library tree; a content-based rust_binary with shared libraries is not supported on Windows yet"
        )
    if isinstance(shlib_tree, Artifact):
        symlinks[shlib_tree.short_path] = shlib_tree

    return make_content_based_dist(ctx, name, exe, copies = copies, symlinks = symlinks)

def _rust_binary_common(
    ctx: AnalysisContext,
    compile_ctx: CompileContext,
    default_roots: list[str],
    extra_flags: list[str],
    allow_cache_upload: bool,
) -> (list[Provider], cmd_args):
    toolchain_info = compile_ctx.toolchain_info

    simple_crate = attr_simple_crate_for_filenames(ctx)

    link_strategy = LinkStrategy(ctx.attrs.link_style) if ctx.attrs.link_style else DEFAULT_STATIC_LINK_STRATEGY
    link_strategy = process_link_strategy_for_pic_behavior(link_strategy, compile_ctx.cxx_toolchain_info.pic_behavior)

    cxx_deps = cxx_attr_deps(ctx)
    generated_build_info_enabled = getattr(ctx.attrs, "_generated_build_info_enabled", False)
    generated_build_info_args = ctx.attrs._generated_build_info_args if generated_build_info_enabled else []

    generated_build_info_link_args = []
    generated_build_info_link_objects = []

    resources = flatten_dict(
        gather_resources(
            label = ctx.label,
            resources = rust_attr_resources(ctx),
            deps = cxx_deps,
        ).values()
    )

    extra_flags = toolchain_info.rustc_binary_flags + (extra_flags or [])

    strategy_param = _strategy_params(ctx, compile_ctx)

    params = strategy_param[link_strategy]
    name = output_filename(compile_ctx, simple_crate, Emit("link"), params)

    # Under advanced_unstable_linking, the binary is compiled via
    # `Emit("rlib")` and linked through cxx below (`rust_link_binary`),
    # mirroring how rust_library produces dylibs via `rust_link_shared`.
    links_via_cxx = toolchain_info.advanced_unstable_linking
    bin_emit = Emit("rlib") if links_via_cxx else Emit("link")

    use_bolt = cxx_use_bolt(ctx)
    if use_bolt and not links_via_cxx:
        fail(
            "`bolt_profile` is set, but BOLT runs as part of the cxx link. "
            + "This target links via rustc; BOLT requires `advanced_unstable_linking` on the Rust toolchain."
        )
    elf_sections = get_elf_sections(ctx)
    if elf_sections and not links_via_cxx:
        fail(
            "`elf_sections` is set, but the sections are added as part of the cxx link. "
            + "This target links via rustc; `elf_sections` requires `advanced_unstable_linking` on the Rust toolchain."
        )
    enable_late_build_info_stamping = cxx_stamp_build_info(ctx)
    content_based_output = getattr(ctx.attrs, "has_content_based_path", False)
    unstamped_name = None
    if enable_late_build_info_stamping:
        allow_cache_upload = True
        unstamped_name = output_filename(compile_ctx, simple_crate, Emit("link"), params, "-unstamped")

    # The exe output is declared below, once we know whether it ships in a
    # content-based `copied_dir` bundle (see the `will_bundle` decision after
    # `shared_libs` is computed).

    build_graph_info = new_build_graph_info(ctx, cxx_deps)
    transformation_spec_context = build_transformation_spec_context(ctx, build_graph_info)

    rust_cxx_link_group_info = inherited_rust_cxx_link_group_info(
        ctx,
        compile_ctx.dep_ctx,
        link_strategy = link_strategy,
        transformation_spec_context = transformation_spec_context,
    )

    # Link groups repartition the dependency graph's code — including the
    # Rust code — into group libraries, filtering the main link accordingly.
    # Without advanced_unstable_linking, rustc links the full dep rlib set no
    # matter what, so the grouped code would be linked twice; this cannot be
    # made correct.
    if rust_cxx_link_group_info != None and not toolchain_info.advanced_unstable_linking:
        fail("link groups on Rust binaries require `advanced_unstable_linking`")
    if rust_cxx_link_group_info != None:
        link_group_mappings = rust_cxx_link_group_info.link_group_info.mappings
        link_group_libs = rust_cxx_link_group_info.link_group_libs
        link_group_preferred_linkage = rust_cxx_link_group_info.link_group_preferred_linkage
        labels_to_links_map = rust_cxx_link_group_info.labels_to_links_map
        targets_consumed_by_link_groups = rust_cxx_link_group_info.targets_consumed_by_link_groups
        filtered_targets = rust_cxx_link_group_info.filtered_targets
    else:
        link_group_mappings = {}
        link_group_libs = {}
        link_group_preferred_linkage = {}
        labels_to_links_map = {}
        targets_consumed_by_link_groups = {}
        filtered_targets = []

    native_link_args = get_inherited_link_args(
        ctx,
        compile_ctx,
        params.dep_link_strategy,
        rust_cxx_link_group_info,
        transformation_spec_context,
    )

    runtime_dep_handling = _get_runtime_dependency_handling(ctx, compile_ctx)

    shlib_deps = []
    if link_strategy == LinkStrategy("shared") or rust_cxx_link_group_info != None:
        shlib_deps = inherited_shared_libs(ctx, compile_ctx.dep_ctx)
    elif runtime_dep_handling == RuntimeDependencyHandling("symlink"):
        # Include all transitive runtime shared library deps in a symlink tree.
        # Only collect libs that would actually be built as shared libraries
        # given the current link strategy, matching cxx_binary behavior.
        linkable_graphs_tset = inherited_linkable_graphs(ctx, compile_ctx.dep_ctx)
        all_graphs = []
        for graphs in linkable_graphs_tset.traverse():
            all_graphs.extend(graphs)
        linkable_graph = create_linkable_graph(ctx, deps = all_graphs)
        for linkable_node in linkable_graph.nodes.traverse():
            if linkable_node.linkable == None:
                continue
            preferred_linkage = linkable_node.linkable.preferred_linkage
            output_style = get_lib_output_style(link_strategy, preferred_linkage, PicBehavior("supported"))
            if output_style == LibOutputStyle("shared_lib") and not linkable_node.linkable.stub:
                shlib_deps.append(merge_shared_libraries(ctx.actions, node = linkable_node.linkable.shared_libs))

    shlib_info = merge_shared_libraries(ctx.actions, deps = shlib_deps)

    link_group_ctx = LinkGroupContext(
        link_group_mappings = link_group_mappings,
        link_group_libs = link_group_libs,
        link_group_preferred_linkage = link_group_preferred_linkage,
        labels_to_links_map = labels_to_links_map,
        targets_consumed_by_link_groups = targets_consumed_by_link_groups,
    )

    # Gather and setup symlink tree of transitive shared library deps.
    shared_libs = build_shared_libs_for_symlink_tree(
        use_link_groups = rust_cxx_link_group_info != None,
        link_group_ctx = link_group_ctx,
        link_strategy = link_strategy,
        shared_libraries = traverse_shared_library_info(shlib_info, transformation_provider = transformation_spec_context),
        extra_shared_libraries = [],
    )

    # Decide whether the content-based output ships as a bare exe or a
    # `assembled_dir` bundle (see `_create_content_based_dist`). A binary with
    # adjacent files -- resources and/or a shared-lib tree -- goes in a bundle,
    # and its exe must stay config-based: a content-based exe sits at a deep
    # content-hash path, so the linker bakes a long buck-out-relative `$ORIGIN`
    # RPATH that no longer points at the shlib tree once the exe is copied into
    # the bundle. A config-based exe is a direct sibling of the tree, yielding a
    # clean `$ORIGIN/<tree>` RPATH that survives co-location; the bundle dir
    # provides the content-addressing instead.
    # `needs_shlib_tree` = a shlib symlink-tree (and a baked `$ORIGIN/<tree>`
    # RPATH) was produced for this exe. That is exactly when `shared_libs` is
    # non-empty: `link_strategy == shared`, a link-group binary (auto link groups
    # are on by DEFAULT, so `link_strategy == shared` alone is NOT sufficient --
    # a static_pic link-group binary still gets a tree + RPATH), or
    # `runtime_dependency_handling = "symlink"` dlopen deps. In every such case
    # the exe carries an `$ORIGIN/<tree>` RPATH, so a content-based (deep-hash)
    # exe would bake a dangling RPATH; bundle, and keep the exe config-based, so
    # the RPATH resolves against the co-located tree. A binary with NO shared
    # libs (e.g. the fully-static node) has no tree/RPATH and stays a bare
    # content-based exe.
    needs_shlib_tree = bool(shared_libs)
    will_bundle = content_based_output and (bool(resources) or needs_shlib_tree)

    # The bare exe stays content-based -- so it is trample-safe when consumed by
    # path or as another target's resource (e.g. clio's `:web-server` embeds the
    # node) -- EXCEPT when it needs a shlib tree at startup. A content-based exe
    # sits at a deep content-hash path, so the linker bakes a long
    # buck-out-relative `$ORIGIN` RPATH that no longer points at the tree once
    # the exe is copied into the bundle. Dynamic binaries therefore use a
    # config-based exe (a direct sibling of the tree => clean `$ORIGIN/<tree>`
    # RPATH that survives co-location) and get content-addressing from the
    # bundle dir instead. Static binaries reuse their content-based exe in the
    # bundle directly (no RPATH to break).
    exe_content_based = content_based_output and not needs_shlib_tree and not use_bolt
    if links_via_cxx:
        # cxx performs the terminal link (see `rust_link_binary` below) and its
        # own post-link processing, so the output has to be named the way those
        # stages expect; we must not stamp again here.
        predeclared_output = ctx.actions.declare_output(
            output_filename(compile_ctx, simple_crate, Emit("link"), params, get_cxx_post_link_suffix(ctx)),
            has_content_based_path = exe_content_based,
        )

        # final_output is whatever cxx returns, set after the link below.
        final_output = None
        # rpath and symlink-tree are computed against the pre-stamped output
        shlib_args_output = predeclared_output
    elif enable_late_build_info_stamping:
        predeclared_output = ctx.actions.declare_output(unstamped_name, has_content_based_path = exe_content_based)
        final_output = ctx.actions.declare_output(name, has_content_based_path = exe_content_based)
        shlib_args_output = final_output
    else:
        # If not using late build info stamping, then the output will be stamped eagerly in rust_compile
        predeclared_output = ctx.actions.declare_output(name, has_content_based_path = exe_content_based)
        final_output = predeclared_output
        shlib_args_output = final_output

    # link groups shared libraries link args are directly added to the link command,
    # we don't have to add them here
    executable_shlib_args = executable_shared_lib_arguments(
        ctx,
        compile_ctx.cxx_toolchain_info,
        shlib_args_output,
        shared_libs,
    )
    if generated_build_info_enabled and not links_via_cxx:
        generated_build_info = generate_build_info(
            ctx,
            generator_args = generated_build_info_args,
            invalidation_inputs = [
                rust_compile_invalidation_inputs(
                    ctx = ctx,
                    compile_ctx = compile_ctx,
                    emit = bin_emit,
                    params = params,
                    default_roots = default_roots,
                    incremental_enabled = ctx.attrs.incremental_enabled,
                ),
                unpack_link_args(native_link_args),
                executable_shlib_args.extra_link_args,
            ],
        )
        if generated_build_info:
            generated_build_info_link_args.extend(generated_build_info.linker_flags)
            generated_build_info_link_args.extend(compile_generated_build_info(ctx, generated_build_info).objects)
    extra_link_args = executable_shlib_args.extra_link_args + generated_build_info_link_args

    # Compile rust binary. Under `Emit("rlib")`, this only compiles: rustc's
    # synthesized objects are extracted and linked below.
    link = rust_compile(
        ctx = ctx,
        compile_ctx = compile_ctx,
        emit = bin_emit,
        params = params,
        default_roots = default_roots,
        extra_link_args = extra_link_args,
        predeclared_output = None if links_via_cxx else predeclared_output,
        predeclared_output_has_content_based_path = exe_content_based,
        extra_flags = extra_flags,
        allow_cache_upload = allow_cache_upload,
        transformation_spec_context = transformation_spec_context,
        incremental_enabled = ctx.attrs.incremental_enabled,
        precomputed_inherited_link_args = native_link_args,
    )

    dwp_output = link.link_output.dwp_output if link.link_output else None
    pdb_output = link.link_output.pdb if link.link_output else None
    prebolt_output = None
    if links_via_cxx:
        rust_link_inputs = [
            link.link_extraction.out_argsfile,
            link.link_extraction.out_artifacts_dir,
            link.link_extraction.out_manifest,
        ]
        if link.link_extraction.out_archive != None:
            rust_link_inputs.append(link.link_extraction.out_archive)
        if generated_build_info_enabled:
            generated_build_info = generate_build_info(
                ctx,
                generator_args = generated_build_info_args,
                invalidation_inputs = rust_link_inputs
                + [
                    unpack_link_args(native_link_args),
                    executable_shlib_args.extra_link_args,
                ],
            )
            if generated_build_info:
                generated_build_info_link_args.extend(generated_build_info.linker_flags)
                generated_build_info_link_objects.extend(compile_generated_build_info(ctx, generated_build_info).objects)
        link_result = rust_link_binary(
            ctx = ctx,
            compile_ctx = compile_ctx,
            extraction = link.link_extraction,
            dep_link_strategy = params.dep_link_strategy,
            reloc_model = params.reloc_model,
            extra_link_args = executable_shlib_args.extra_link_args,
            binary_link_args = generated_build_info_link_objects + generated_build_info_link_args,
            inherited_link_args = native_link_args,
            dwo_output_directory = link.compile_output.dwo_output_directory,
            output = predeclared_output,
            output_has_content_based_path = exe_content_based,
            identifier = name,
            allow_cache_upload = allow_cache_upload,
        )
        final_output = link_result.linked_object.output
        dwp_output = link_result.linked_object.dwp
        pdb_output = link_result.linked_object.pdb
        prebolt_output = link_result.linked_object.prebolt_output
    elif enable_late_build_info_stamping:
        stamp_build_info(ctx, link.output, final_output)

    args = cmd_args(final_output, hidden = executable_shlib_args.runtime_files)
    external_debug_info = project_artifacts(
        actions = ctx.actions,
        tsets = inherited_external_debug_info(
            ctx,
            compile_ctx.dep_ctx,
            link_strategy,
        ),
    )
    if link.compile_output.dwo_output_directory:
        external_debug_info.append(link.compile_output.dwo_output_directory)

    # If we have some resources, write it to the resources JSON file and add
    # it and all resources to "runtime_files" so that we make to materialize
    # them with the final binary.
    runtime_files = list(executable_shlib_args.runtime_files)
    relocatable_resources_json = None
    relocatable_resources_contents = None
    if resources:
        resources_hidden = [
            create_resource_db(
                ctx = ctx,
                name = name + ".resources.json",
                binary = final_output,
                resources = resources,
            )
        ]
        for resource in resources.values():
            resources_hidden.append(resource.default_output)
            resources_hidden.extend(resource.other_outputs)
        args.add(cmd_args(hidden = resources_hidden))
        runtime_files.extend(resources_hidden)
        relocatable_resources_json, relocatable_resources_contents = create_relocatable_resources_info(
            ctx = ctx,
            name = name,
            resources = resources,
            has_content_based_path = content_based_output,
        )

    # If opted into content-based output and the binary has adjacent files
    # (resources and/or a shared-lib tree), package everything into a single
    # content-based `assembled_dir` and point both the run command and
    # `DefaultInfo.default_output` at the exe projected out of the bundle, so
    # by-path and resource consumers get the self-contained, co-located form.
    dist_bundle = None
    dist_exe = None
    if will_bundle:
        dist_bundle, dist_exe = _create_content_based_dist(
            ctx = ctx,
            name = name,
            exe = final_output,
            relocatable_resources_json = relocatable_resources_json,
            relocatable_resources_contents = relocatable_resources_contents,
            shlib_tree = executable_shlib_args.shared_libs_symlink_tree,
        )
        # Run the bundled exe (projected out of the content-based bundle). The
        # bundle carries the co-located resources / shlib tree; `runtime_files`
        # is still added so a resource's own `other_outputs` (e.g. a resource
        # that is itself a dynamic exe) materialize too, mirroring the
        # non-bundle contract.
        args = cmd_args(dist_exe, hidden = [dist_bundle] + runtime_files)

    # A simple dict of sub-target key to artifact, which we'll convert to
    # DefaultInfo providers at the end
    extra_compiled_targets = {
        "sources": compile_ctx.symlinked_srcs,
    }
    sub_targets = {}

    # TODO(agallagher) There appears to be pre-existing soname conflicts
    # when building this (when using link groups), which prevents using
    # `with_unique_str_sonames`.
    str_soname_shlibs = {shlib.soname.ensure_str(): shlib for shlib in shared_libs if shlib.soname.is_str}
    sub_targets["shared-libraries"] = [
        DefaultInfo(
            default_output = ctx.actions.write_json(
                name + ".shared-libraries.json",
                {
                    "libraries": ["{}:{}[shared-libraries][{}]".format(ctx.label.path, ctx.label.name, soname) for soname in str_soname_shlibs],
                    "librariesdwp": [
                        "{}:{}[shared-libraries][{}][dwp]".format(ctx.label.path, ctx.label.name, soname)
                        for soname, shlib in str_soname_shlibs.items()
                        if shlib.lib.dwp
                    ],
                    "rpathtree": ["{}:{}[rpath-tree]".format(ctx.label.path, ctx.label.name)] if executable_shlib_args.shared_libs_symlink_tree else [],
                },
                has_content_based_path = False,
            ),
            sub_targets = {
                soname: [
                    DefaultInfo(
                        default_output = shlib.lib.output,
                        sub_targets = {"dwp": [DefaultInfo(default_output = shlib.lib.dwp)]} if shlib.lib.dwp else {},
                    )
                ]
                for soname, shlib in str_soname_shlibs.items()
            },
        )
    ]

    if isinstance(executable_shlib_args.shared_libs_symlink_tree, Artifact):
        sub_targets["rpath-tree"] = [
            DefaultInfo(
                default_output = executable_shlib_args.shared_libs_symlink_tree,
                other_outputs = [shlib.lib.output for shlib in shared_libs] + [shlib.lib.dwp for shlib in shared_libs if shlib.lib.dwp],
            )
        ]

    if rust_cxx_link_group_info:
        sub_targets[LINK_GROUP_MAP_DATABASE_SUB_TARGET] = [get_link_group_map_json(ctx, filtered_targets)]
        readable_mappings = {}
        for node, group in link_group_mappings.items():
            readable_mappings[group] = readable_mappings.get(group, []) + ["{}//{}:{}".format(node.cell, node.package, node.name)]
        sub_targets[LINK_GROUP_MAPPINGS_SUB_TARGET] = [
            DefaultInfo(
                default_output = ctx.actions.write_json(
                    name + LINK_GROUP_MAPPINGS_FILENAME_SUFFIX,
                    readable_mappings,
                    has_content_based_path = False,
                ),
            )
        ]

    # `infallible_diagnostics` allows us to circumvent compilation failures and
    # treat the resulting rustc action as a success, even if a metadata
    # artifact was not generated. This allows us to generate diagnostics
    # even when the target has bugs.
    diag_artifacts = {}
    clippy_artifacts = {}
    for incr in (True, False):
        diag_artifacts[incr] = rust_compile(
            ctx = ctx,
            compile_ctx = compile_ctx,
            emit = Emit("metadata-fast"),
            params = strategy_param[DEFAULT_STATIC_LINK_STRATEGY],
            default_roots = default_roots,
            extra_flags = extra_flags,
            infallible_diagnostics = True,
            incremental_enabled = incr,
            transformation_spec_context = transformation_spec_context,
        )
        clippy_artifacts[incr] = rust_compile(
            ctx = ctx,
            compile_ctx = compile_ctx,
            emit = Emit("clippy"),
            params = strategy_param[DEFAULT_STATIC_LINK_STRATEGY],
            default_roots = default_roots,
            extra_flags = extra_flags,
            infallible_diagnostics = True,
            incremental_enabled = incr,
            transformation_spec_context = transformation_spec_context,
        )

    providers = [
        RustcExtraOutputsInfo(
            metadata = diag_artifacts[False],
            metadata_incr = diag_artifacts[True],
            clippy = clippy_artifacts[False],
            clippy_incr = clippy_artifacts[True],
            remarks = None,  # Exposed via subtargets, not this provider
        )
    ]

    incr_enabled = ctx.attrs.incremental_enabled
    extra_compiled_targets.update(output_as_diag_subtargets(diag_artifacts[incr_enabled], clippy_artifacts[incr_enabled]))

    # Add remarks subtargets (lazy - only built when subtarget requested)
    # Uses `params` to match the actual binary's link strategy
    remarks = rust_compile(
        ctx = ctx,
        compile_ctx = compile_ctx,
        emit = bin_emit,
        params = params,
        default_roots = default_roots,
        extra_link_args = extra_link_args,
        extra_flags = extra_flags,
        incremental_enabled = False,
        profile_mode = ProfileMode("remarks"),
        transformation_spec_context = transformation_spec_context,
    )
    if remarks.compile_output.remarks_txt:
        extra_compiled_targets["remarks.txt"] = remarks.compile_output.remarks_txt
    if remarks.compile_output.remarks_json:
        extra_compiled_targets["remarks.json"] = remarks.compile_output.remarks_json

    if compile_ctx.toolchain_info.nightly_features:
        # `-Zunpretty=expanded` is unstable
        extra_compiled_targets["expand"] = rust_compile(
            ctx = ctx,
            compile_ctx = compile_ctx,
            emit = Emit("expand"),
            params = strategy_param[DEFAULT_STATIC_LINK_STRATEGY],
            default_roots = default_roots,
            extra_flags = extra_flags,
            incremental_enabled = ctx.attrs.incremental_enabled,
        ).output

    llvm_ir_noopt = rust_compile(
        ctx = ctx,
        compile_ctx = compile_ctx,
        emit = Emit("llvm-ir-noopt"),
        params = strategy_param[DEFAULT_STATIC_LINK_STRATEGY],
        default_roots = default_roots,
        extra_flags = extra_flags,
        incremental_enabled = ctx.attrs.incremental_enabled,
    ).output
    if compile_ctx.toolchain_info.nightly_features:
        llvm_time_trace = rust_compile(
            ctx = ctx,
            compile_ctx = compile_ctx,
            emit = bin_emit,
            params = params,
            default_roots = default_roots,
            extra_link_args = extra_link_args,
            extra_flags = extra_flags,
            incremental_enabled = ctx.attrs.incremental_enabled,
            profile_mode = ProfileMode("llvm-time-trace"),
        )
        self_profile = rust_compile(
            ctx = ctx,
            compile_ctx = compile_ctx,
            emit = bin_emit,
            params = params,
            default_roots = default_roots,
            extra_link_args = extra_link_args,
            extra_flags = extra_flags,
            incremental_enabled = ctx.attrs.incremental_enabled,
            profile_mode = ProfileMode("self-profile"),
        )
    else:
        # `-Zllvm-time-trace` and `-Zself-profile` are unstable
        llvm_time_trace = None
        self_profile = None
    profiles = make_profile_providers(
        ctx = ctx,
        compile_ctx = compile_ctx,
        llvm_ir_noopt = llvm_ir_noopt,
        llvm_time_trace = llvm_time_trace,
        self_profile = self_profile,
    )
    sub_targets["profile"] = profiles

    for emit_type in ["asm", "llvm-ir", "mir"]:
        extra_compiled_targets[emit_type] = rust_compile(
            ctx = ctx,
            compile_ctx = compile_ctx,
            emit = Emit(emit_type),
            params = params,
            default_roots = default_roots,
            extra_flags = extra_flags,
            incremental_enabled = ctx.attrs.incremental_enabled,
        ).output

    doc_output = generate_rustdoc(
        ctx = ctx,
        compile_ctx = compile_ctx,
        params = strategy_param[DEFAULT_STATIC_LINK_STRATEGY],
        default_roots = default_roots,
        document_private_items = True,
    )
    extra_compiled_targets["doc"] = doc_output

    named_deps_names = write_named_deps_names(ctx, compile_ctx)
    if named_deps_names:
        extra_compiled_targets["named_deps"] = named_deps_names

    if dwp_output:
        sub_targets["dwp"] = [
            DefaultInfo(
                default_output = dwp_output,
                other_outputs = [shlib.lib.dwp for shlib in shared_libs if shlib.lib.dwp]
                + ([executable_shlib_args.dwp_symlink_tree] if executable_shlib_args.dwp_symlink_tree else []),
            ),
        ]

    if pdb_output:
        sub_targets[PDB_SUB_TARGET] = get_pdb_providers(pdb = pdb_output, binary = final_output)

    if use_bolt and prebolt_output:
        sub_targets["prebolt"] = [DefaultInfo(default_output = prebolt_output)]

    dupmbin_toolchain = compile_ctx.cxx_toolchain_info.dumpbin_toolchain_path
    if dupmbin_toolchain:
        sub_targets[DUMPBIN_SUB_TARGET] = get_dumpbin_providers(ctx, final_output, dupmbin_toolchain)

    sub_targets.update({k: [DefaultInfo(default_output = v)] for (k, v) in extra_compiled_targets.items()})

    if dist_bundle:
        sub_targets["dist"] = [DefaultInfo(default_output = dist_bundle)]

    providers += [
        DefaultInfo(
            default_output = dist_exe if dist_bundle else final_output,
            other_outputs = runtime_files
            + (executable_shlib_args.external_debug_info + external_debug_info if compile_ctx.cxx_toolchain_info.materialize_external_debug_info else [])
            + ([dist_bundle] if dist_bundle else []),
            sub_targets = sub_targets,
        ),
        DistInfo(
            shared_libs = shlib_info.set,
            # The bundle is load-bearing at runtime for a bundled binary: the
            # projected exe resolves its manifest / shlib tree as bundle
            # siblings, so dist consumers that materialize only
            # `nondebug_runtime_files` must get the bundle too.
            nondebug_runtime_files = runtime_files + ([dist_bundle] if dist_bundle else []),
            relocatable_resources_contents = relocatable_resources_contents,
            relocatable_resources_json = relocatable_resources_json,
        ),
    ]
    providers.append(
        rust_analyzer_provider(
            ctx = ctx,
            compile_ctx = compile_ctx,
            default_roots = default_roots,
        )
    )
    return (providers, args)

def rust_binary_impl(ctx: AnalysisContext) -> list[Provider]:
    compile_ctx = compile_context(ctx, binary = True)

    providers, args = _rust_binary_common(
        ctx = ctx,
        compile_ctx = compile_ctx,
        default_roots = ["main.rs"],
        extra_flags = [],
        allow_cache_upload = cxx_attrs_get_allow_cache_upload(ctx.attrs),
    )

    return providers + [RunInfo(args = args)]

def _rust_test_remote_profile(ctx):
    # The remote-test profile that applies to this test, or None: its own
    # remote_execution (a profile name or a property dict), or, when that is
    # None, the remote-test toolchain's default_profile. "disabled" names none.
    remote_execution = ctx.attrs.remote_execution
    remote_toolchain = ctx.attrs._remote_test_execution_toolchain
    if remote_execution == "disabled":
        return None
    if remote_execution != None:
        if type(remote_execution) == "dict":
            return remote_execution
        if remote_toolchain == None:
            fail("{}: remote_execution {} names a profile, but there is no remote-test toolchain".format(ctx.label, repr(remote_execution)))
        profiles = remote_toolchain[RemoteTestExecutionToolchainInfo].profiles
        if remote_execution not in profiles:
            fail("{}: remote_execution names unknown remote-test profile {}".format(ctx.label, repr(remote_execution)))
        if profiles[remote_execution] == None:
            fail("{}: remote_execution names remote-test profile {}, which has no properties".format(ctx.label, repr(remote_execution)))
        return profiles[remote_execution]
    if remote_toolchain == None:
        return None
    return remote_toolchain[RemoteTestExecutionToolchainInfo].default_profile

# Worker-local paths a remote libtest test gets unless the target sets them.
# Both are project-relative, so the runner resolves them inside the action.
_REMOTE_TEST_PATH_DEFAULTS = {"HOME": ".", "TMPDIR": "."}
_PROJECT_RELATIVE_ENV = "--project-relative-env"

def _rust_test_runtime(ctx, toolchain_info, remote):
    # The test's environment and the runner arguments that make its
    # project-relative path variables absolute where it runs.
    environment = ctx.attrs.env | ctx.attrs.run_env
    names = []
    for name in ctx.attrs.path_run_env:
        if name not in environment:
            fail("{}: path_run_env names {}, which neither env nor run_env sets".format(ctx.label, name))
        if name in names:
            fail("{}: path_run_env names {} twice".format(ctx.label, name))
        names.append(name)
    if remote and ctx.attrs.framework:
        defaults = {name: value for name, value in _REMOTE_TEST_PATH_DEFAULTS.items() if name not in environment}
        environment = defaults | environment
        names = [name for name in _REMOTE_TEST_PATH_DEFAULTS if name not in names] + names

    # A name the target already passes the runner in its own args keeps that
    # spelling and is not passed twice; the runner refuses a repeated name.
    args = ctx.attrs.args
    passed = [args[i + 1] for i in range(len(args) - 1) if args[i] == _PROJECT_RELATIVE_ENV]
    names = [name for name in names if name not in passed]

    if names and (not ctx.attrs.framework or toolchain_info.test_process_runner == None):
        fail("{}: project-relative run_env {} needs the libtest process runner".format(ctx.label, names))
    arguments = []
    for name in names:
        arguments.extend([_PROJECT_RELATIVE_ENV, name])
    return environment, arguments

def _rust_test_re_executors(ctx, toolchain_info, profile, environment, runner_arguments):
    # A rust_test runs wherever its remote-test profile sends it. A consumer makes
    # remote execution the default by setting the remote-test toolchain's
    # default_profile; a test that cannot run remotely says remote_execution =
    # "disabled". With no profile at all, re_utils gives the stock local executor.
    if profile == None:
        return get_re_executors_from_props(ctx)

    if type(profile) != "dict":
        fail("{}: a remote rust_test profile must be a property dict, not {}".format(ctx.label, repr(profile)))

    # No local fallback: a remote test's profile may not also enable this host.
    local_enabled = profile.get("local_enabled", False)
    local_listing_enabled = profile.get("local_listing_enabled", False)
    if type(local_enabled) != "bool" or local_enabled or type(local_listing_enabled) != "bool" or local_listing_enabled:
        fail("{}: a remote rust_test profile must not enable local execution or listing".format(ctx.label))

    # A remote profile names where it runs.
    capabilities = profile.get("capabilities")
    if type(capabilities) != "dict" or not capabilities:
        fail("{}: a remote rust_test profile requires nonempty remote capabilities".format(ctx.label))
    for key, value in capabilities.items():
        if type(key) != "string" or not key or type(value) != "string" or not value:
            fail("{}: a remote rust_test profile requires nonempty string capabilities".format(ctx.label))
    use_case = profile.get("use_case")
    if type(use_case) != "string" or not use_case:
        fail("{}: a remote rust_test profile requires an explicit remote use_case".format(ctx.label))

    # Host secret routes and live effects belong to this host and are never sent
    # to a worker.
    if toolchain_info.test_secret_read_route != None or "unqualified-live-effect" in ctx.attrs.labels:
        fail("{}: a remote rust_test must not carry the host secret route or live effects; set remote_execution = \"disabled\"".format(ctx.label))
    for name in ["MUNINN_SECRET_READ_ROUTE", "OP_CONNECT_HOST"]:
        if name in environment:
            fail("{}: a remote rust_test must not carry host-route environment {}".format(ctx.label, name))

    # Hermeticity: the worker runs the declared runtime under the per-test runner,
    # which resolves worker-local HOME and TMPDIR for both external listing and
    # the runner's internal listing.
    if not ctx.attrs.framework or toolchain_info.test_process_runner == None:
        fail("{}: a remote rust_test requires the libtest process runner; a custom harness sets remote_execution = \"disabled\"".format(ctx.label))
    if toolchain_info.trial_nix_manifest == None:
        fail("{}: a remote rust_test requires the declared runtime manifest".format(ctx.label))
    if ctx.attrs.framework:
        # _rust_test_runtime supplies both unless the target sets its own value.
        # Runtime expected values preserve attrs.arg literal equality; constant-string
        # comparisons use a Starlark opcode that rejects resolved argument values.
        for name, expected in _REMOTE_TEST_PATH_DEFAULTS.items():
            if environment.get(name) != expected:
                fail("{}: a remote rust_test requires project-relative HOME/TMPDIR".format(ctx.label))
        passed = runner_arguments + ctx.attrs.args
        for name in _REMOTE_TEST_PATH_DEFAULTS:
            if not any([passed[i] == _PROJECT_RELATIVE_ENV and passed[i + 1] == name for i in range(len(passed) - 1)]):
                fail("{}: a remote rust_test requires project-relative HOME/TMPDIR".format(ctx.label))

    executors = get_re_executors_from_props(ctx)
    # The resolver can still force a local executor. Do not let it silently
    # downgrade a remote test.
    if not executors.run_from_project_root or not executors.use_project_relative_paths or executors.default_executor == None or "listing" not in executors.executor_overrides:
        fail("{}: remote rust_test did not resolve remote execution and listing".format(ctx.label))
    return executors

def rust_test_impl(ctx: AnalysisContext) -> list[Provider]:
    compile_ctx = compile_context(ctx, binary = True)
    toolchain_info = compile_ctx.toolchain_info

    extra_flags = toolchain_info.rustc_test_flags or []
    if ctx.attrs.framework:
        extra_flags += ["--test"]

    providers, args = _rust_binary_common(
        ctx = ctx,
        compile_ctx = compile_ctx,
        # Unless default_roots are provided, it is ambiguous whether this test rule is invoked
        # to test a binary, or to test a library. As such, we must consider both main.rs and
        # lib.rs as potential candidates.
        default_roots = ctx.attrs.default_roots or ["main.rs", "lib.rs"],
        extra_flags = extra_flags,
        allow_cache_upload = False,
    )

    profile = _rust_test_remote_profile(ctx)
    environment, runner_arguments = _rust_test_runtime(ctx, toolchain_info, profile != None)
    re_executors = _rust_test_re_executors(ctx, toolchain_info, profile, environment, runner_arguments)

    test_command = cmd_args(args, hidden = [toolchain_info.trial_nix_manifest] if toolchain_info.trial_nix_manifest != None else [])

    # Run each libtest test in its own process. A custom harness
    # (`framework = False`) need not speak libtest's listing protocol.
    if toolchain_info.test_process_runner != None and ctx.attrs.framework:
        test_command = cmd_args(toolchain_info.test_process_runner, test_command)

    # Set the host's secret read route for the test, outside the per-test runner so
    # that every test process it starts inherits the route. The prefix reads a host
    # file that is no action input, so a cacheable test, whose result could be
    # reused after that file changes, gets none.
    if toolchain_info.test_secret_read_route != None and not ctx.attrs.supports_test_execution_caching:
        test_command = cmd_args(toolchain_info.test_secret_read_route, test_command)

    # The runner's project-relative variables, then the target's own test
    # arguments, follow the binary; the runner passes the latter to libtest unchanged.
    test_command = cmd_args(test_command, runner_arguments, ctx.attrs.args)

    return (
        inject_test_run_info(
            ctx,
            ExternalRunnerTestInfo(
                type = "rust",
                command = [test_command],
                env = environment,
                labels = ctx.attrs.labels,
                contacts = ctx.attrs.contacts,
                default_executor = re_executors.default_executor,
                executor_overrides = re_executors.executor_overrides,
                run_from_project_root = True,
                use_project_relative_paths = True,
                supports_test_execution_caching = ctx.attrs.supports_test_execution_caching,
            ),
        )
        + providers
    )
