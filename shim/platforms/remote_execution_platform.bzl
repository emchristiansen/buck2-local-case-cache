"""Remote-default Linux execution with explicit local-only action support."""

load("@prelude//cfg/exec_platform:marker.bzl", "get_exec_platform_marker")
load("@prelude//git:git_fetch.bzl", "GIT_FETCH_RUNTIME_MANIFEST")

REMOTE_EXECUTION_PROPERTIES = {
    "OSFamily": "Linux",
    "cpu_count": "2",
}

# The `muninn-test-runtime` capability a test action requires. A worker
# advertises exactly this value only when `muninn-test-runtime-probe
# <work_directory>`, its `query_cmd`, prints it; the probe's
# `TEST_RUNTIME_VALUE` must equal it. The value names a fixed contract, so any
# new fact needs a new version: a stale probe or worker can never match a
# newer profile. `linux-unprivileged-v2` promises, for the worker context and
# its canonical work root W, every one of these facts (each failure is a
# named absence, so the worker advertises nothing for this key):
#
# v1 facts:
# - The effective user is not root, and the effective capability set holds
#   neither CAP_DAC_OVERRIDE nor CAP_DAC_READ_SEARCH.
# - A mode-0500 directory the process owns refuses a new entry with EACCES.
# - W is spelled canonically, and neither W nor any ancestor holds `.git` or
#   `.jj`.
# - W's filesystem supports mkdir, symlink and readlink, chmod (the mode is
#   applied) and fsync of a directory descriptor.
# - Loopback TCP on 127.0.0.1 carries one byte each way.
# - pidfd_open of a spawned child works; its pidfd is not ready while the
#   child is held and is ready once it has exited.
#
# v2 additions:
# - W is at most 39 bytes, or at most 7 when W is under /tmp, /dev/shm or
#   /run (whole path components). NativeLink 1.7.6 runs an action in
#   W/<36-byte UUID operation id>/work with an empty working_directory
#   (running_actions_manager.rs:4556 and :2112), so the action's working
#   directory is len(W) + 42 bytes, and the test runner and its fixtures need
#   it at most 81 bytes, or 49 under those shared roots.
# - Peer identity: a pathname AF_UNIX stream listener in a private directory
#   under W binds (a path too long for sun_path is an absence), and
#   getsockopt(SOL_SOCKET, SO_PEERPIDFD) on an accepted connection yields a
#   pidfd of the connecting child, not ready while the child is held.
#   pidfd_send_signal(SIGKILL) through it makes it ready and the child's
#   status killed by SIGKILL, so the peer pidfd names exactly that child.
# - Lock holder visibility: while that child holds an exclusive flock on a
#   file in its working directory, /proc/locks has a FLOCK line with its pid
#   and the file's inode, /proc/<pid>/fd holds a descriptor on the file's
#   device and inode, and /proc/<pid>/cwd names the directory.
# - Process view: /proc/self/environ reads; /proc/self/stat names getpid()
#   and getppid(); /proc/<ppid>/stat is readable and names the parent; and
#   /proc/sys/kernel/random/boot_id is exactly one lowercase UUID and a
#   newline (37 bytes).
# - Devices: /dev/null reads end of file and accepts a 1-byte write;
#   /dev/zero reads zeros; /dev/full refuses a write with ENOSPC; /dev/urandom
#   fills a 16-byte read; each is a character device.
# - Memfd seals: memfd_create(MFD_CLOEXEC | MFD_ALLOW_SEALING), written and
#   sealed with F_SEAL_WRITE, F_SEAL_GROW, F_SEAL_SHRINK and F_SEAL_SEAL,
#   reports those seals, reopens read-only through /proc/self/fd with its
#   payload, and refuses a write through a fresh /proc/self/fd writer with
#   EPERM.
# - Pipe reopen: a pipe's write end reopens through /proc/self/fd/<fd> with
#   O_WRONLY | O_APPEND | O_NONBLOCK | O_CLOEXEC, and a payload written
#   through it reads back whole from the read end, because test diagnostics
#   reopen a pipe-backed stderr that way (buck2_funnel's write_diagnostic),
#   which a policy can deny while allowing the memfd reopen above.
# - prctl: PR_SET_PDEATHSIG(SIGKILL) and PR_SET_DUMPABLE(0) each take effect
#   and read back; the probe restores both before its verdict.
# - Account record: getpwuid_r(geteuid()) finds a record whose home
#   directory is absolute, as cargo_funnel_launch's account_home requires
#   for private test queues; HOME cannot supply it.
# - Temporary root: /tmp is a directory owned by root or the effective user
#   that is sticky whenever group or others may write it (as mode 1777 is);
#   it is writable; a fresh directory created under it with mode 0700 is
#   owned by the effective user with exactly that mode; and in it a pathname
#   AF_UNIX stream listener binds and listens at
#   org.chromium.Chromium.XXXXXX/SingletonSocket, because browser fixtures
#   keep Chromium's singleton socket there: that 45-byte suffix past a long
#   per-action TMPDIR exceeds the 107 pathname bytes sun_path holds. This
#   privacy is against other users only (actions share the worker's uid),
#   and nothing promises cleanup of a browser's descendants.
#
# Only test profiles require this key; build and FETCH actions keep
# `REMOTE_EXECUTION_PROPERTIES` alone. A scheduler that does not declare the
# key matches every worker, so it is enforced only once the scheduler matches
# it exactly and the worker advertises it after checking each fact.
MUNINN_TEST_RUNTIME = {"muninn-test-runtime": "linux-unprivileged-v2"}

def _remote_execution_platform_impl(ctx):
    constraints = {}
    constraints.update(ctx.attrs.cpu_configuration[ConfigurationInfo].constraints)
    constraints.update(ctx.attrs.os_configuration[ConfigurationInfo].constraints)
    cfg = ConfigurationInfo(constraints = constraints, values = {})

    name = ctx.label.raw_target()
    platform = ExecutionPlatformInfo(
        label = name,
        configuration = cfg,
        executor_config = CommandExecutorConfig(
            local_enabled = True,
            remote_enabled = True,
            use_limited_hybrid = True,
            allow_limited_hybrid_fallbacks = False,
            allow_hybrid_fallbacks_on_failure = False,
            remote_execution_use_case = "buck2-default",
            remote_execution_properties = REMOTE_EXECUTION_PROPERTIES,
            use_windows_path_separators = False,
        ),
    )

    return [
        DefaultInfo(),
        platform,
        PlatformInfo(label = str(name), configuration = cfg),
        ExecutionPlatformRegistrationInfo(
            platforms = [platform, ctx.attrs.fetch_platform[ExecutionPlatformInfo]],
            exec_marker_constraint = get_exec_platform_marker(),
        ),
    ]

remote_execution_platform = rule(
    impl = _remote_execution_platform_impl,
    attrs = {
        "cpu_configuration": attrs.dep(providers = [ConfigurationInfo]),
        "fetch_platform": attrs.dep(providers = [ExecutionPlatformInfo]),
        "os_configuration": attrs.dep(providers = [ConfigurationInfo]),
    },
)

def _remote_fetch_execution_platform_impl(ctx):
    constraints = {}
    constraints.update(ctx.attrs.cpu_configuration[ConfigurationInfo].constraints)
    constraints.update(ctx.attrs.os_configuration[ConfigurationInfo].constraints)
    constraints.update(ctx.attrs.fetch_constraint[ConfigurationInfo].constraints)
    cfg = ConfigurationInfo(constraints = constraints, values = {})
    properties = dict(REMOTE_EXECUTION_PROPERTIES)
    properties["muninn-fetch-runtime"] = GIT_FETCH_RUNTIME_MANIFEST

    name = ctx.label.raw_target()
    platform = ExecutionPlatformInfo(
        label = name,
        configuration = cfg,
        executor_config = CommandExecutorConfig(
            local_enabled = False,
            remote_enabled = True,
            use_limited_hybrid = False,
            allow_limited_hybrid_fallbacks = False,
            allow_hybrid_fallbacks_on_failure = False,
            remote_execution_use_case = "buck2-default",
            remote_execution_properties = properties,
            use_windows_path_separators = False,
        ),
    )
    # Only remote_linux registers platforms, preserving its ordinary platform first.
    return [
        DefaultInfo(),
        platform,
        PlatformInfo(label = str(name), configuration = cfg),
    ]

remote_fetch_execution_platform = rule(
    impl = _remote_fetch_execution_platform_impl,
    attrs = {
        "cpu_configuration": attrs.dep(providers = [ConfigurationInfo]),
        "fetch_constraint": attrs.dep(providers = [ConfigurationInfo]),
        "os_configuration": attrs.dep(providers = [ConfigurationInfo]),
    },
)
