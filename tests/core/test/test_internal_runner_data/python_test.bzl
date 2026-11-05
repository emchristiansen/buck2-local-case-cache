# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load(
    ":case_parsers.bzl",
    "parse_internal_listing",
    "parse_internal_result",
    "parse_internal_result_with_details",
    "parse_named_result",
)

def _impl(ctx):
    if ctx.attrs.internal:
        return [
            DefaultInfo(),
            InternalRunnerTestInfo(
                type = "custom",
                command = ["fbpython", "-c", ctx.attrs.script],
                # Listing gets no fresh directories.
                listing_command = [
                    "fbpython",
                    "-c",
                    "import os; assert not any(n in os.environ for n in {}); print('case')".format(ctx.attrs.fresh_dir_env),
                ],
                parse_test_listing = parse_internal_listing,
                parse_test_result = parse_internal_result,
                supports_test_execution_caching = ctx.attrs.local_case_cache,
                fresh_dir_env = ctx.attrs.fresh_dir_env,
            ),
        ]
    return [
        DefaultInfo(),
        ExternalRunnerTestInfo(
            command = ["fbpython", "-c", ctx.attrs.script],
            type = "custom",
        ),
    ]

python_test = rule(
    impl = _impl,
    attrs = {
        "fresh_dir_env": attrs.list(attrs.string(), default = []),
        "internal": attrs.bool(default = False),
        "local_case_cache": attrs.bool(default = False),
        "script": attrs.string(),
    },
)

# Each provider's command passes only when that provider is the expected one and runs with its own
# environment. Only the Internal provider declares `INTERNAL_ONLY`, so running the wrong provider,
# or pairing one provider's command or environment with the other's, fails.
_PAIR_SCRIPT = "import os, sys; sys.exit(0 if '{provider}' == '{expected}' and os.environ.get('{own}') == '1' and '{other}' not in os.environ else 1)"

def _pair_command(provider, expected, own, other):
    return [
        "fbpython",
        "-c",
        _PAIR_SCRIPT.format(provider = provider, expected = expected, own = own, other = other),
    ]

def _provider_pair_impl(ctx):
    expected = ctx.attrs.expected
    return [
        DefaultInfo(),
        InternalRunnerTestInfo(
            type = "custom",
            command = _pair_command("internal", expected, "INTERNAL_ONLY", "EXTERNAL_ONLY"),
            env = {"INTERNAL_ONLY": "1"},
            labels = ["internal_provider"],
            listing_command = ["fbpython", "-c", "print('case')"],
            parse_test_listing = parse_internal_listing,
            parse_test_result = parse_internal_result,
        ),
        ExternalRunnerTestInfo(
            command = _pair_command("external", expected, "EXTERNAL_ONLY", "INTERNAL_ONLY"),
            env = {"EXTERNAL_ONLY": "1"},
            labels = ["external_provider"],
            type = "custom",
        ),
    ]

provider_pair_test = rule(
    impl = _provider_pair_impl,
    attrs = {
        # `internal` or `external`: the provider the test run is expected to use.
        "expected": attrs.string(),
    },
)

_READS_EXTERNAL = "import sys; sys.exit(0 if open(sys.argv[1]).read() == 'external' else 1)"

# Each provider's command reads a generated input of its own. The Internal provider's input comes
# from an action that always fails, so building the Internal provider's artifacts fails the
# target; the External provider's command passes only if its own distinct input was built.
def _provider_artifact_pair_impl(ctx):
    internal_input = ctx.actions.declare_output("internal_input.txt")
    ctx.actions.run(
        cmd_args("fbpython", "-c", "import sys; sys.exit(1)", hidden = internal_input.as_output()),
        category = "internal_only_input",
    )
    external_input = ctx.actions.write("external_input.txt", "external")
    return [
        DefaultInfo(),
        InternalRunnerTestInfo(
            type = "custom",
            command = ["fbpython", "-c", _READS_EXTERNAL, internal_input],
            listing_command = ["fbpython", "-c", "print('case')"],
            parse_test_listing = parse_internal_listing,
            parse_test_result = parse_internal_result,
        ),
        ExternalRunnerTestInfo(
            command = ["fbpython", "-c", _READS_EXTERNAL, external_input],
            type = "custom",
        ),
    ]

provider_artifact_pair_test = rule(
    impl = _provider_artifact_pair_impl,
    attrs = {},
)

# A case of `fresh_dir_case_test` that holds its scratch path. The first run of the case to start
# claims it, records its pid and sleeps. A later run of the same case records whether that
# process still runs and passes only if it does not: its scratch path, removed and recreated
# for it, must not have been released while the holder's child was alive.
_HOLD = """
import os, sys, time
case = sys.argv[-1]
print(case, flush = True)
signals = os.path.join(os.environ["SIGNALS"], case)
os.makedirs(signals, exist_ok=True)
fresh = os.environ["FRESH_A"]
assert fresh == os.path.join(os.environ["TMPDIR"], "0"), fresh
assert os.listdir(fresh) == [], os.listdir(fresh)

def running(pid):
    try:
        with open("/proc/%d/stat" % pid) as f:
            return f.read().rsplit(")", 1)[1].split()[0] != "Z"
    except FileNotFoundError:
        return False

try:
    claim = os.open(os.path.join(signals, "claim"), os.O_CREAT | os.O_EXCL | os.O_WRONLY)
except FileExistsError:
    with open(os.path.join(signals, "holder")) as f:
        holder = int(f.read())
    state = "running" if running(holder) else "gone"
    with open(os.path.join(signals, "queued"), "w") as f:
        f.write(state)
    assert state == "gone", "the holder %d still runs in this scratch path" % holder
    sys.exit(0)
os.close(claim)
with open(os.path.join(fresh, "mine"), "w") as f:
    f.write("holder")
with open(os.path.join(signals, "holder.tmp"), "w") as f:
    f.write(str(os.getpid()))
os.rename(os.path.join(signals, "holder.tmp"), os.path.join(signals, "holder"))
time.sleep(60)
"""

# A case that, on its first run, waits until its sibling `hold` holds and then prints output its
# parser refuses, which ends the test run while `hold` still runs. Later runs pass.
_FAIL_PARSER = """
import os, sys, time
case = sys.argv[-1]
signals = os.path.join(os.environ["SIGNALS"], case)
os.makedirs(signals, exist_ok=True)
try:
    os.close(os.open(os.path.join(signals, "claim"), os.O_CREAT | os.O_EXCL | os.O_WRONLY))
except FileExistsError:
    print(case)
    sys.exit(0)
holder = os.path.join(os.environ["SIGNALS"], "hold", "holder")
while not os.path.exists(holder):
    time.sleep(0.1)
print("UNPARSEABLE")
"""

def _fresh_dir_case_impl(ctx):
    script = "import sys; exec({{'hold': {hold}, 'fail_parser': {fail}}}[sys.argv[-1]])".format(
        hold = repr(_HOLD),
        fail = repr(_FAIL_PARSER),
    )
    if ctx.attrs.self_contained_parser:
        parse_test_result = parse_named_result
    else:
        parse_test_result = lambda stdout, stderr, exit_code: parse_named_result(stdout, stderr, exit_code)
    return [
        DefaultInfo(),
        InternalRunnerTestInfo(
            type = "custom",
            command = ["fbpython", "-c", script],
            env = {"SIGNALS": ctx.attrs.signals},
            listing_command = ["fbpython", "-c", "print({})".format(repr("\n".join(ctx.attrs.cases)))],
            parse_test_listing = parse_internal_listing,
            parse_test_result = parse_test_result,
            supports_test_execution_caching = ctx.attrs.local_case_cache,
            fresh_dir_env = ["FRESH_A"],
        ),
    ]

fresh_dir_case_test = rule(
    impl = _fresh_dir_case_impl,
    attrs = {
        "cases": attrs.list(attrs.string()),
        "local_case_cache": attrs.bool(default = False),
        "self_contained_parser": attrs.bool(default = True),
        # Where the cases record what they observed: a directory outside every scratch path.
        "signals": attrs.string(),
    },
)

# Each declared name must reach the case as its own new, empty directory, whatever else the
# executor or a local resource would set it to.
_CHECK_FRESH = """
import json, os
tmp = os.environ["TMPDIR"]
names = json.loads(os.environ["FRESH_NAMES"])
with open(os.environ["STARTED"], "w") as f:
    f.write(json.dumps({n: os.environ.get(n) for n in names + ["OTHER"]}))
assert sorted(os.listdir(tmp)) == [str(i) for i in range(len(names))], os.listdir(tmp)
for i, name in enumerate(names):
    assert os.environ[name] == os.path.join(tmp, str(i)), (name, os.environ[name])
    assert os.listdir(os.environ[name]) == [], name
expected_other = os.environ["EXPECTED_OTHER"]
if expected_other:
    assert os.environ.get("OTHER") == expected_other, os.environ.get("OTHER")
print("case")
"""

def _broker_impl(ctx):
    resources = ctx.actions.write_json("resources.json", {"resources": [{"alias": "42"}]})
    return [
        DefaultInfo(),
        LocalResourceInfo(
            setup = cmd_args(["cat", resources]),
            resource_env_vars = {ctx.attrs.variable: "alias"},
            setup_timeout_seconds = 5,
        ),
    ]

fresh_dir_broker = rule(impl = _broker_impl, attrs = {"variable": attrs.string()})

def _fresh_dir_env_impl(ctx):
    env = {
        "EXPECTED_OTHER": "42" if ctx.attrs.broker and ctx.attrs.broker_sets_other else "",
        "FRESH_NAMES": json.encode(ctx.attrs.fresh_dir_env),
        "STARTED": ctx.attrs.started,
    }
    resources = {}
    if ctx.attrs.broker:
        resources = {
            "local_resources": {"fresh_dir_resource": ctx.attrs.broker.label},
            "required_local_resources": [RequiredTestLocalResource("fresh_dir_resource")],
        }
    return [
        DefaultInfo(),
        InternalRunnerTestInfo(
            type = "custom",
            command = ["fbpython", "-c", _CHECK_FRESH],
            env = env,
            listing_command = ["fbpython", "-c", "print('case')"],
            parse_test_listing = parse_internal_listing,
            parse_test_result = parse_internal_result_with_details,
            fresh_dir_env = ctx.attrs.fresh_dir_env,
            **resources
        ),
    ]

fresh_dir_env_test = rule(
    impl = _fresh_dir_env_impl,
    attrs = {
        "broker": attrs.option(attrs.dep(providers = [LocalResourceInfo]), default = None),
        "broker_sets_other": attrs.bool(default = False),
        "fresh_dir_env": attrs.list(attrs.string()),
        # A file the case writes when it starts, outside its scratch path.
        "started": attrs.string(),
    },
)
