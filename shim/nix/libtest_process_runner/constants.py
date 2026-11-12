"""Fixed values shared by the per-test process runner."""

# libtest's own exit status when any test fails; the runner reports the same.
LIBTEST_FAILURE_EXIT = 101

# Exit status for a runner invocation it cannot interpret.
USAGE_EXIT = 2

# Shared or memory-backed temporary roots a run's temporary directory never goes
# under. /run holds session runtime directories, which a Buck2 daemon's TMPDIR may
# name.
SHARED_TEMPORARY_ROOTS = (b"/tmp", b"/dev/shm", b"/run")

# Under HOME, where a run's temporary directory goes when the runner's own TMPDIR is
# unset or under a shared root. The same rule `cargo funnel run` applies.
FALLBACK_TEMPORARY_ROOT = b".cache/libtest-process-runner/t"

# How long the runner keeps reading a finished test's output while processes it
# left behind still hold the pipe open. This is nextest's default leak-timeout,
# which the repository's nextest configuration leaves in force.
LEAK_TIMEOUT_SECONDS = 0.1

# The libtest summary line each single-test process prints last.
LIBTEST_SUMMARY_PATTERN = (
    r"^test result: (?:ok|FAILED)\. (?P<passed>\d+) passed; (?P<failed>\d+) failed; "
    r"(?P<ignored>\d+) ignored; (?P<measured>\d+) measured; \d+ filtered out"
)

# libtest's terse listing line: `<name>: test` or `<name>: bench`.
LIBTEST_LISTING_PATTERN = r"^(?P<name>.+): (?:test|bench)$"

# The option naming an executable whose directory the runner puts first on the PATH
# of each process it starts.
PATH_PREFIX_OF_EXE = "--path-prefix-of-exe"

# The option that runs one selected test as libtest itself runs it and hands on that
# process's own output and exit status unchanged; see `run_raw_one_case`.
RAW_ONE_CASE = "--raw-one-case"

# The runner's exit status when it killed a raw one-case test at its timeout, as
# timeout(1) reports. libtest never exits with it.
RAW_ONE_CASE_TIMEOUT_EXIT = 124
