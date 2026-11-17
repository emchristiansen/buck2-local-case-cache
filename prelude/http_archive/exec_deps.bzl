# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

HttpArchiveExecDeps = provider(
    fields = {
        "archive_shell": provider_field(typing.Any),
        "archive_shell_info": provider_field(typing.Any),
        "exec_os_type": provider_field(typing.Any, default = None),
    }
)

PinnedExecutableInfo = provider(fields = {
    "nix_manifest": provider_field(typing.Any),
    "path_env": provider_field(typing.Any),
})

def _http_archive_exec_deps_impl(ctx: AnalysisContext) -> list[Provider]:
    archive_shell = ctx.attrs.archive_shell
    return [
        DefaultInfo(),
        HttpArchiveExecDeps(
            archive_shell = archive_shell[RunInfo],
            archive_shell_info = archive_shell[PinnedExecutableInfo],
            exec_os_type = ctx.attrs.exec_os_type,
        ),
    ]

http_archive_exec_deps = rule(
    impl = _http_archive_exec_deps_impl,
    attrs = {
        "archive_shell": attrs.exec_dep(
            providers = [RunInfo, PinnedExecutableInfo],
            default = "toolchains//:archive_shell",
        ),
        "exec_os_type": attrs.default_only(attrs.dep(default = "prelude//os_lookup/targets:os_lookup")),
    },
)
