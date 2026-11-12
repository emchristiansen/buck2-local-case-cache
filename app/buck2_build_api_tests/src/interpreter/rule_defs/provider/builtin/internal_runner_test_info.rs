/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use buck2_build_api::interpreter::rule_defs::provider::builtin::internal_runner_test_info::InternalRunnerTestInfo;
use buck2_build_api::interpreter::rule_defs::provider::builtin::internal_runner_test_info::OwnedInternalRunnerTestInfo;
use buck2_build_api::interpreter::rule_defs::register_rule_defs;
use buck2_build_api::interpreter::rule_defs::required_test_local_resource::register_required_test_local_resource;
use buck2_core::bzl::ImportPath;
use buck2_interpreter_for_build::interpreter::testing::Tester;
use buck2_interpreter_for_build::interpreter::testing::cells;
use buck2_test_api::data::TestStatus;
use indoc::indoc;

fn tester() -> Tester {
    let mut tester = Tester::new().unwrap();
    tester.additional_globals(register_rule_defs);
    tester.additional_globals(register_required_test_local_resource);
    tester
}

fn freeze_provider(starlark_code: &str) -> buck2_error::Result<OwnedInternalRunnerTestInfo> {
    let mut tester = tester();
    let loaded = tester.add_import(
        &ImportPath::testing_new("root//test:provider.bzl"),
        starlark_code,
    )?;
    loaded
        .env()
        .get("exported_info")
        .expect("`exported_info` not found")
        .downcast_starlark::<InternalRunnerTestInfo>()
        .map_err(buck2_error::Error::from)
}

/// A generic self-contained libtest parser module: top-level defs with literal defaults,
/// docstrings and helpers, using only standard Starlark builtins and methods.
const GENERIC_LIBTEST_PARSERS: &str = r#"
"""A libtest listing and single-case result parser that loads nothing."""

def _listed_name(line, suffix = ": test"):
    """The case name on one terse listing line, or None for anything else."""
    if line.endswith(suffix):
        return line[:-len(suffix)]
    return None

def parse_libtest_listing(stdout):
    names = [_listed_name(line) for line in stdout.split("\n")]
    return [{"name": name, "filter": name} for name in names if name]

def _status(word):
    return {"FAILED": "FAIL", "ignored": "SKIP", "ok": "PASS"}.get(word.split(",")[0])

def parse_libtest_case(stdout, stderr, exit_code):
    results = []
    for index, line in enumerate(stdout.split("\n")):
        if not line.startswith("test ") or " ... " not in line:
            continue
        name, _separator, word = line[len("test "):].partition(" ... ")
        status = _status(word)
        if status == None:
            fail("line {} has an unknown outcome: {}".format(index, repr(word)))
        results.append({"name": name, "status": status, "message": None if exit_code == 0 else stderr})
    if len(results) != 1 or int(exit_code) not in range(256):
        return []
    return results
"#;

/// The native internal-runner fixture's self-contained parsers,
/// `tests/core/test/test_internal_runner_data/case_parsers.bzl`.
const FIXTURE_CASE_PARSERS: &str = r#"# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

"""Self-contained internal-runner parsers: a module of top-level defs that loads nothing."""

def parse_internal_listing(stdout):
    return [{"name": name, "filter": name} for name in stdout.strip().split("\n") if name]

def parse_internal_result(stdout, stderr, exit_code):
    return [{
        "name": "case",
        "status": "PASS" if exit_code == 0 else "FAIL",
        "message": None if exit_code == 0 else stderr,
    }]
"#;

/// The real Buck loader with `config` as the root cell's buckconfig: `parsers` is loaded from
/// `//test:parsers.bzl`, and the provider names its `listing` and `result` functions.
fn provider_with_parsers(
    config: Option<&str>,
    parsers: &str,
    listing: &str,
    result: &str,
) -> buck2_error::Result<OwnedInternalRunnerTestInfo> {
    let mut tester = Tester::with_cells(cells(config)?)?;
    tester.additional_globals(register_rule_defs);
    tester.additional_globals(register_required_test_local_resource);
    tester.add_import(&ImportPath::testing_new("root//test:parsers.bzl"), parsers)?;
    let loaded = tester.add_import(
        &ImportPath::testing_new("root//test:provider.bzl"),
        &format!(
            r#"
load("//test:parsers.bzl", "{listing}", "{result}")
exported_info = InternalRunnerTestInfo(
    type = "rust",
    parse_test_listing = {listing},
    parse_test_result = {result},
    listing_command = ["binary", "--list"],
    supports_test_execution_caching = True,
)
"#
        ),
    )?;
    loaded
        .env()
        .get("exported_info")
        .expect("`exported_info` not found")
        .downcast_starlark::<InternalRunnerTestInfo>()
        .map_err(buck2_error::Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_cache_permission_is_not_inferred_from_rust_type() -> buck2_error::Result<()> {
        let info = freeze_provider(indoc!(
            r#"
        exported_info = InternalRunnerTestInfo(
            type = "rust",
            parse_test_listing = lambda stdout: [],
            parse_test_result = lambda stdout, stderr, exit_code: [],
            listing_command = ["binary", "--list"],
        )
        "#
        ))?;
        assert!(
            !info
                .as_ref()
                .value()
                .as_ref()
                .supports_test_execution_caching()
        );
        Ok(())
    }

    #[test]
    fn internal_cache_permission_requires_explicit_opt_in() -> buck2_error::Result<()> {
        let info = freeze_provider(indoc!(
            r#"
        exported_info = InternalRunnerTestInfo(
            type = "rust",
            parse_test_listing = lambda stdout: [],
            parse_test_result = lambda stdout, stderr, exit_code: [],
            listing_command = ["binary", "--list"],
            supports_test_execution_caching = True,
        )
        "#
        ))?;
        assert!(
            info.as_ref()
                .value()
                .as_ref()
                .supports_test_execution_caching()
        );
        Ok(())
    }

    #[test]
    fn internal_cache_permission_rejects_non_boolean_values() {
        assert!(
            freeze_provider(indoc!(
                r#"
            exported_info = InternalRunnerTestInfo(
                type = "rust",
                parse_test_listing = lambda stdout: [],
                parse_test_result = lambda stdout, stderr, exit_code: [],
                listing_command = ["binary", "--list"],
                supports_test_execution_caching = "true",
            )
            "#
            ))
            .is_err()
        );
    }

    // ---------------------------------------------------------------------------
    // Self-contained parser identity, loaded by the real Buck interpreter
    // ---------------------------------------------------------------------------

    const READ_CONFIG_PARSERS: &str = r#"
def parse_listing(stdout):
    return [{"name":"case","filter":"case"}]
def parse_result(stdout, stderr, exit_code):
    return [{"name":"case","status":read_config("probe", "status", "PASS")}]
"#;

    /// A parser that calls Buck's `read_config` parses the same output differently under two
    /// configurations, so its source is not its identity: it is ineligible under both.
    #[test]
    fn self_contained_parser_identity_rejects_config_dependent_parser() -> buck2_error::Result<()> {
        let mut statuses = Vec::new();
        for status in ["PASS", "FAIL"] {
            let info = provider_with_parsers(
                Some(&format!("[probe]\nstatus = {status}\n")),
                READ_CONFIG_PARSERS,
                "parse_listing",
                "parse_result",
            )?;
            let identity = info
                .as_ref()
                .value()
                .as_ref()
                .self_contained_parser_identity();
            let reason = identity.expect_err("a configuration-dependent parser has no identity");
            assert!(
                reason.contains("read_config")
                    && reason.contains("neither its own nor a standard Starlark builtin"),
                "{reason}"
            );
            let results = InternalRunnerTestInfo::parse_test_result_output(&info, "", "", 0)?;
            statuses.push(results[0].status.clone());
        }
        // The hazard is real: identical source and output, different results.
        assert_eq!(statuses, [TestStatus::PASS, TestStatus::FAIL]);
        Ok(())
    }

    /// A self-contained libtest parser module is admitted, with an identity that is its source
    /// and that does not depend on the configuration, and it still parses a libtest case.
    #[test]
    fn self_contained_parser_identity_admits_generic_libtest_parsers() -> buck2_error::Result<()>
    {
        let mut identities = Vec::new();
        for config in [None, Some("[probe]\nstatus = FAIL\n")] {
            let info = provider_with_parsers(
                config,
                GENERIC_LIBTEST_PARSERS,
                "parse_libtest_listing",
                "parse_libtest_case",
            )?;
            let identity = info
                .as_ref()
                .value()
                .as_ref()
                .self_contained_parser_identity()
                .map_err(|e| buck2_error::buck2_error!(buck2_error::ErrorTag::Input, "{e}"))?;
            assert!(identity.contains(GENERIC_LIBTEST_PARSERS), "{identity}");
            assert!(identity.starts_with("parse_test_listing: "), "{identity}");
            identities.push(identity);

            let listing = InternalRunnerTestInfo::parse_test_listing_output(
                &info,
                "a::b: test\nc: benchmark\n",
            )?;
            assert_eq!(listing.len(), 1);
            assert_eq!(listing[0].name, "a::b");
            let results = InternalRunnerTestInfo::parse_test_result_output(
                &info,
                "\nrunning 1 test\ntest a::b ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n",
                "",
                0,
            )?;
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].name, "a::b");
            assert_eq!(results[0].status, TestStatus::PASS);
        }
        assert_eq!(identities[0], identities[1]);
        Ok(())
    }

    /// The native fixture's parsers stay admitted too.
    #[test]
    fn self_contained_parser_identity_admits_fixture_parsers() -> buck2_error::Result<()> {
        let info = provider_with_parsers(
            None,
            FIXTURE_CASE_PARSERS,
            "parse_internal_listing",
            "parse_internal_result",
        )?;
        let identity = info
            .as_ref()
            .value()
            .as_ref()
            .self_contained_parser_identity();
        assert!(
            identity
                .as_ref()
                .is_ok_and(|i| i.contains(FIXTURE_CASE_PARSERS)),
            "{identity:?}"
        );
        Ok(())
    }

    /// A lambda is not a top-level function of a self-contained file.
    #[test]
    fn self_contained_parser_identity_rejects_lambda() -> buck2_error::Result<()> {
        let info = freeze_provider(indoc!(
            r#"
        exported_info = InternalRunnerTestInfo(
            type = "rust",
            parse_test_listing = lambda stdout: [],
            parse_test_result = lambda stdout, stderr, exit_code: [],
            listing_command = ["binary", "--list"],
        )
        "#
        ))?;
        assert!(
            info.as_ref()
                .value()
                .as_ref()
                .self_contained_parser_identity()
                .is_err()
        );
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Construction: both callbacks are required alongside `type`
    // ---------------------------------------------------------------------------

    #[test]
    fn test_minimal_construction() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test(indoc!(
            r#"
        def test():
            InternalRunnerTestInfo(
                type = "custom",
                parse_test_listing = lambda stdout: [],
                parse_test_result = lambda stdout, stderr, exit_code: [],
                listing_command = ["binary", "--list"],
            )
        "#
        ))?;
        Ok(())
    }

    #[test]
    fn test_construction_with_optional_fields() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test(indoc!(
            r#"
        def test():
            InternalRunnerTestInfo(
                type = "custom",
                parse_test_listing = lambda stdout: [],
                parse_test_result = lambda stdout, stderr, exit_code: [],
                listing_command = ["my_test_binary", "--list"],
                command = ["my_test_binary"],
                env = {"FOO": "bar"},
                labels = ["slow", "integration"],
                contacts = ["oncall+my_team@xmail.facebook.com"],
                use_project_relative_paths = True,
                run_from_project_root = False,
            )
        "#
        ))?;
        Ok(())
    }

    #[test]
    fn test_construction_with_cmd_args() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test(indoc!(
            r#"
        def test():
            InternalRunnerTestInfo(
                type = "custom",
                parse_test_listing = lambda stdout: [],
                parse_test_result = lambda stdout, stderr, exit_code: [],
                listing_command = ["binary", "--list"],
                command = ["binary", cmd_args()],
                env = {"KEY": cmd_args()},
            )
        "#
        ))?;
        Ok(())
    }

    #[test]
    fn test_construction_with_local_resources() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test(indoc!(
            r#"
        def test():
            InternalRunnerTestInfo(
                type = "custom",
                parse_test_listing = lambda stdout: [],
                parse_test_result = lambda stdout, stderr, exit_code: [],
                listing_command = ["binary", "--list"],
                local_resources = {"gpu": None},
                required_local_resources = [RequiredTestLocalResource("gpu", listing=False)],
            )
        "#
        ))?;
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Validation: missing required parameters
    // ---------------------------------------------------------------------------

    #[test]
    fn test_missing_type() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                )
            "#
            ),
            "Missing required parameter",
        );
        Ok(())
    }

    #[test]
    fn test_missing_parse_test_listing() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                )
            "#
            ),
            "Missing required parameter",
        );
        Ok(())
    }

    #[test]
    fn test_missing_parse_test_result() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    listing_command = ["binary", "--list"],
                )
            "#
            ),
            "Missing required parameter",
        );
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Validation: listing_command
    // ---------------------------------------------------------------------------

    #[test]
    fn test_missing_listing_command() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                )
            "#
            ),
            "Missing required parameter",
        );
        Ok(())
    }

    #[test]
    fn test_listing_command_empty_validation() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = [],
                )
            "#
            ),
            "`listing_command` must be non-empty",
        );
        Ok(())
    }

    #[test]
    fn test_listing_command_with_cmd_args() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test(indoc!(
            r#"
        def test():
            InternalRunnerTestInfo(
                type = "custom",
                parse_test_listing = lambda stdout: [],
                parse_test_result = lambda stdout, stderr, exit_code: [],
                listing_command = ["binary", cmd_args(), "--list"],
            )
        "#
        ))?;
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Validation: wrong types for required parameters
    // ---------------------------------------------------------------------------

    #[test]
    fn test_type_must_be_string() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = 123,
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                )
            "#
            ),
            "`type`",
        );
        Ok(())
    }

    #[test]
    fn test_parse_test_listing_must_be_callable() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = "not_a_function",
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                )
            "#
            ),
            "`parse_test_listing`",
        );
        Ok(())
    }

    #[test]
    fn test_parse_test_result_must_be_callable() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = "not_a_function",
                    listing_command = ["binary", "--list"],
                )
            "#
            ),
            "`parse_test_result`",
        );
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Validation: wrong types for optional parameters
    // ---------------------------------------------------------------------------

    #[test]
    fn test_command_validation() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    command = "not_a_list",
                )
            "#
            ),
            "`command`",
        );

        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    command = [123],
                )
            "#
            ),
            "`command`",
        );
        Ok(())
    }

    #[test]
    fn test_env_validation() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    env = "not_a_dict",
                )
            "#
            ),
            "`env`",
        );

        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    env = {"key": 123},
                )
            "#
            ),
            "`env`",
        );
        Ok(())
    }

    /// Each `fresh_dir_env` name is a variable name that nothing else sets.
    #[test]
    fn test_fresh_dir_env_validation() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    env = {},
                    fresh_dir_env = ["1FRESH"],
                )
            "#
            ),
            "`fresh_dir_env` entry `1FRESH` is not a valid environment variable name",
        );
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    env = {},
                    fresh_dir_env = ["FRESH-DIR"],
                )
            "#
            ),
            "`fresh_dir_env` entry `FRESH-DIR` is not a valid environment variable name",
        );
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    env = {},
                    fresh_dir_env = [""],
                )
            "#
            ),
            "`fresh_dir_env` entry `` is not a valid environment variable name",
        );
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    env = {},
                    fresh_dir_env = ["PWD"],
                )
            "#
            ),
            "`fresh_dir_env` entry `PWD` is set by the executor",
        );
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    env = {},
                    fresh_dir_env = ["FRESH", "OTHER", "FRESH"],
                )
            "#
            ),
            "`fresh_dir_env` entry `FRESH` is declared more than once",
        );
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    env = {"HOME": "/home"},
                    fresh_dir_env = ["HOME"],
                )
            "#
            ),
            "`fresh_dir_env` entry `HOME` is also a key of `env`",
        );
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    env = {},
                    fresh_dir_env = "FRESH",
                )
            "#
            ),
            "fresh_dir_env",
        );
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    env = {},
                    fresh_dir_env = [1],
                )
            "#
            ),
            "fresh_dir_env",
        );
        Ok(())
    }

    /// The declared names, in declaration order; none by default. `TMPDIR` and `HOME` are
    /// allowed: the request environment replaces what the executor set.
    #[test]
    fn test_fresh_dir_env_accessor() -> buck2_error::Result<()> {
        let info = freeze_provider(indoc!(
            r#"
        exported_info = InternalRunnerTestInfo(
            type = "custom",
            parse_test_listing = lambda stdout: [],
            parse_test_result = lambda stdout, stderr, exit_code: [],
            listing_command = ["binary", "--list"],
            env = {"OTHER": "1"},
            fresh_dir_env = ["TMPDIR", "HOME", "_X9"],
        )
        "#
        ))?;
        assert_eq!(
            info.as_ref().value().as_ref().fresh_dir_env().collect::<Vec<_>>(),
            ["TMPDIR", "HOME", "_X9"]
        );
        let info = freeze_provider(indoc!(
            r#"
        exported_info = InternalRunnerTestInfo(
            type = "custom",
            parse_test_listing = lambda stdout: [],
            parse_test_result = lambda stdout, stderr, exit_code: [],
            listing_command = ["binary", "--list"],
        )
        "#
        ))?;
        assert_eq!(info.as_ref().value().as_ref().fresh_dir_env().count(), 0);
        Ok(())
    }

    #[test]
    fn test_labels_validation() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    labels = "not_a_list",
                )
            "#
            ),
            "`labels`",
        );

        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    labels = [123],
                )
            "#
            ),
            "`labels`",
        );
        Ok(())
    }

    #[test]
    fn test_contacts_validation() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    contacts = "not_a_list",
                )
            "#
            ),
            "`contacts`",
        );

        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    contacts = [123],
                )
            "#
            ),
            "`contacts`",
        );
        Ok(())
    }

    #[test]
    fn test_bool_field_validation() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    use_project_relative_paths = "yes",
                )
            "#
            ),
            "`use_project_relative_paths`",
        );

        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    run_from_project_root = "yes",
                )
            "#
            ),
            "`run_from_project_root`",
        );
        Ok(())
    }

    #[test]
    fn test_executor_overrides_validation() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    executor_overrides = {"foo": "not_an_executor"},
                )
            "#
            ),
            "`executor_overrides`",
        );
        Ok(())
    }

    #[test]
    fn test_default_executor_validation() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    default_executor = "not_an_executor",
                )
            "#
            ),
            "Expected type `CommandExecutorConfig | None` but got `str`",
        );
        Ok(())
    }

    #[test]
    fn test_required_local_resources_validation() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    required_local_resources = ["not_a_resource"],
                )
            "#
            ),
            "`required_local_resources` should only contain `RequiredTestLocalResource` values",
        );

        tester.run_starlark_bzl_test_expecting_error(
            indoc!(
                r#"
            def test():
                InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    required_local_resources = [RequiredTestLocalResource("gpu")],
                )
            "#
            ),
            "`required_local_resources` contains `gpu` which is not present in `local_resources`",
        );
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Freeze-time validation: mutation after construction
    // ---------------------------------------------------------------------------

    #[test]
    fn test_validation_at_freeze_contacts_mutation() -> buck2_error::Result<()> {
        let mut tester = tester();
        let res = tester.add_import(
            &ImportPath::testing_new("root//test:def1.bzl"),
            indoc!(
                r#"
            def make_info():
                contacts = []
                info = InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    contacts = contacts,
                )
                contacts.append(123)
                return info

            exported_info = make_info()
            "#
            ),
        );
        assert!(res.is_err());
        Ok(())
    }

    #[test]
    fn test_validation_at_freeze_labels_mutation() -> buck2_error::Result<()> {
        let mut tester = tester();
        let res = tester.add_import(
            &ImportPath::testing_new("root//test:def2.bzl"),
            indoc!(
                r#"
            def make_info():
                labels = []
                info = InternalRunnerTestInfo(
                    type = "custom",
                    parse_test_listing = lambda stdout: [],
                    parse_test_result = lambda stdout, stderr, exit_code: [],
                    listing_command = ["binary", "--list"],
                    labels = labels,
                )
                labels.append(123)
                return info

            exported_info = make_info()
            "#
            ),
        );
        assert!(res.is_err());
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // parse_test_listing callback: functional behavior via Starlark
    // ---------------------------------------------------------------------------

    #[test]
    fn test_parse_test_listing_callback_returns_list() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test(indoc!(
            r#"
        def my_listing_parser(stdout):
            results = []
            for line in stdout.strip().split("\n"):
                if line:
                    results.append({"name": line, "filter": line})
            return results

        def my_result_parser(stdout, stderr, exit_code):
            return []

        def test():
            info = InternalRunnerTestInfo(
                type = "custom",
                parse_test_listing = my_listing_parser,
                parse_test_result = my_result_parser,
                listing_command = ["binary", "--list"],
            )
            # Verify the provider can be constructed with a real function
            assert_true(isinstance(info, InternalRunnerTestInfo))
        "#
        ))?;
        Ok(())
    }

    #[test]
    fn test_parse_test_listing_with_name_and_filter() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test(indoc!(
            r#"
        def my_listing_parser(stdout):
            return [
                {
                    "name": "test_foo",
                    "filter": "module.TestClass.test_foo",
                },
            ]

        def noop_result(stdout, stderr, exit_code):
            return []

        def test():
            info = InternalRunnerTestInfo(
                type = "python",
                parse_test_listing = my_listing_parser,
                parse_test_result = noop_result,
                listing_command = ["binary", "--list"],
            )
            assert_true(isinstance(info, InternalRunnerTestInfo))
        "#
        ))?;
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // parse_test_result callback: functional behavior via Starlark
    // ---------------------------------------------------------------------------

    #[test]
    fn test_parse_test_result_callback_returns_list() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test(indoc!(
            r#"
        def my_result_parser(stdout, stderr, exit_code):
            results = []
            if exit_code == 0:
                results.append({
                    "name": "test_all",
                    "status": "PASS",
                })
            else:
                results.append({
                    "name": "test_all",
                    "status": "FAIL",
                    "message": "Non-zero exit code: " + str(exit_code),
                })
            return results

        def noop_listing(stdout):
            return []

        def test():
            info = InternalRunnerTestInfo(
                type = "custom",
                parse_test_listing = noop_listing,
                parse_test_result = my_result_parser,
                listing_command = ["binary", "--list"],
            )
            assert_true(isinstance(info, InternalRunnerTestInfo))
        "#
        ))?;
        Ok(())
    }

    #[test]
    fn test_parse_test_result_with_all_fields() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test(indoc!(
            r#"
        def my_result_parser(stdout, stderr, exit_code):
            return [
                {
                    "name": "test_addition",
                    "status": "PASS",
                    "message": None,
                    "duration": 0.123,
                    "details": "All assertions passed",
                },
                {
                    "name": "test_subtraction",
                    "status": "FAIL",
                    "message": "Expected 3, got 4",
                    "duration": 1.5,
                    "details": stderr,
                },
            ]

        def noop_listing(stdout):
            return []

        def test():
            info = InternalRunnerTestInfo(
                type = "custom",
                parse_test_listing = noop_listing,
                parse_test_result = my_result_parser,
                listing_command = ["binary", "--list"],
            )
            assert_true(isinstance(info, InternalRunnerTestInfo))
        "#
        ))?;
        Ok(())
    }

    #[test]
    fn test_parse_test_result_with_integer_duration() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test(indoc!(
            r#"
        def my_result_parser(stdout, stderr, exit_code):
            return [
                {
                    "name": "test_slow",
                    "status": "PASS",
                    "duration": 5,
                },
            ]

        def noop_listing(stdout):
            return []

        def test():
            info = InternalRunnerTestInfo(
                type = "custom",
                parse_test_listing = noop_listing,
                parse_test_result = my_result_parser,
                listing_command = ["binary", "--list"],
            )
            assert_true(isinstance(info, InternalRunnerTestInfo))
        "#
        ))?;
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Realistic end-to-end callback: JSON-based test runner
    // ---------------------------------------------------------------------------

    #[test]
    fn test_json_based_listing_and_result_parsers() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test(indoc!(
            r#"
        def parse_json_listing(stdout):
            tests = json.decode(stdout)
            results = []
            for t in tests:
                results.append({
                    "name": t["name"],
                    "filter": t.get("filter", t["name"]),
                })
            return results

        def parse_json_result(stdout, stderr, exit_code):
            results = json.decode(stdout)
            entries = []
            for r in results:
                entries.append({
                    "name": r["name"],
                    "status": r["status"],
                    "message": r.get("message", None),
                    "duration": r.get("duration", None),
                    "details": r.get("details", None),
                })
            return entries

        def test():
            info = InternalRunnerTestInfo(
                type = "json_runner",
                parse_test_listing = parse_json_listing,
                parse_test_result = parse_json_result,
                listing_command = ["binary", "--list"],
            )
            assert_true(isinstance(info, InternalRunnerTestInfo))
        "#
        ))?;
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Realistic end-to-end callback: line-based test runner
    // ---------------------------------------------------------------------------

    #[test]
    fn test_line_based_listing_parser() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test(indoc!(
            r#"
        def parse_line_listing(stdout):
            entries = []
            for line in stdout.strip().split("\n"):
                line = line.strip()
                if line:
                    entries.append({"name": line, "filter": line})
            return entries

        def parse_exit_code_result(stdout, stderr, exit_code):
            if exit_code == 0:
                return [{"name": "suite", "status": "PASS"}]
            else:
                return [{"name": "suite", "status": "FAIL", "message": stderr}]

        def test():
            info = InternalRunnerTestInfo(
                type = "line_runner",
                parse_test_listing = parse_line_listing,
                parse_test_result = parse_exit_code_result,
                listing_command = ["run_tests", "--list"],
                command = ["run_tests"],
            )
            assert_true(isinstance(info, InternalRunnerTestInfo))
        "#
        ))?;
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Provider identity: isinstance checks
    // ---------------------------------------------------------------------------

    #[test]
    fn test_isinstance_checks() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test(indoc!(
            r#"
        def test():
            info = InternalRunnerTestInfo(
                type = "custom",
                parse_test_listing = lambda stdout: [],
                parse_test_result = lambda stdout, stderr, exit_code: [],
                listing_command = ["binary", "--list"],
            )
            assert_true(isinstance(info, InternalRunnerTestInfo))
            assert_true(isinstance(info, Provider))
            assert_false(isinstance(info, DefaultInfo))
            assert_false(isinstance(info, ExternalRunnerTestInfo))
        "#
        ))?;
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Multiple status values in parse_test_result
    // ---------------------------------------------------------------------------

    #[test]
    fn test_parse_test_result_various_statuses() -> buck2_error::Result<()> {
        let mut tester = tester();
        tester.run_starlark_bzl_test(indoc!(
            r#"
        def my_result_parser(stdout, stderr, exit_code):
            return [
                {"name": "test_pass", "status": "PASS"},
                {"name": "test_fail", "status": "FAIL", "message": "assertion error"},
                {"name": "test_skip", "status": "SKIP", "message": "not applicable"},
                {"name": "test_timeout", "status": "TIMEOUT"},
                {"name": "test_omitted", "status": "OMITTED"},
                {"name": "test_fatal", "status": "FATAL", "message": "crash"},
                {"name": "test_listing_success", "status": "LISTING_SUCCESS"},
                {"name": "test_listing_fail", "status": "LISTING_FAILED"},
            ]

        def noop_listing(stdout):
            return []

        def test():
            info = InternalRunnerTestInfo(
                type = "custom",
                parse_test_listing = noop_listing,
                parse_test_result = my_result_parser,
                listing_command = ["binary", "--list"],
            )
            assert_true(isinstance(info, InternalRunnerTestInfo))
        "#
        ))?;
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Rust-side callback invocation: parse_test_listing_output
    // ---------------------------------------------------------------------------

    #[test]
    fn test_parse_test_listing_output_basic() -> buck2_error::Result<()> {
        let info = freeze_provider(indoc!(
            r#"
        def my_listing(stdout):
            results = []
            for line in stdout.strip().split("\n"):
                if line:
                    results.append({"name": line, "filter": line})
            return results

        exported_info = InternalRunnerTestInfo(
            type = "custom",
            parse_test_listing = my_listing,
            parse_test_result = lambda stdout, stderr, exit_code: [],
            listing_command = ["binary", "--list"],
        )
        "#
        ))?;

        let entries =
            InternalRunnerTestInfo::parse_test_listing_output(&info, "test_foo\ntest_bar\n")?;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "test_foo");
        assert_eq!(entries[0].filter, "test_foo");
        assert_eq!(entries[1].name, "test_bar");
        assert_eq!(entries[1].filter, "test_bar");
        Ok(())
    }

    #[test]
    fn test_parse_test_listing_output_empty() -> buck2_error::Result<()> {
        let info = freeze_provider(indoc!(
            r#"
        exported_info = InternalRunnerTestInfo(
            type = "custom",
            parse_test_listing = lambda stdout: [],
            parse_test_result = lambda stdout, stderr, exit_code: [],
            listing_command = ["binary", "--list"],
        )
        "#
        ))?;

        let entries = InternalRunnerTestInfo::parse_test_listing_output(&info, "")?;
        assert_eq!(entries.len(), 0);
        Ok(())
    }

    #[test]
    fn test_parse_test_listing_output_name_differs_from_filter() -> buck2_error::Result<()> {
        let info = freeze_provider(indoc!(
            r#"
        def my_listing(stdout):
            return [
                {"name": "Test Addition", "filter": "math::TestAddition"},
                {"name": "Test Subtraction", "filter": "math::TestSubtraction"},
            ]

        exported_info = InternalRunnerTestInfo(
            type = "custom",
            parse_test_listing = my_listing,
            parse_test_result = lambda stdout, stderr, exit_code: [],
            listing_command = ["binary", "--list"],
        )
        "#
        ))?;

        let entries = InternalRunnerTestInfo::parse_test_listing_output(&info, "ignored")?;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "Test Addition");
        assert_eq!(entries[0].filter, "math::TestAddition");
        Ok(())
    }

    #[test]
    fn test_parse_test_listing_output_missing_key() -> buck2_error::Result<()> {
        let info = freeze_provider(indoc!(
            r#"
        exported_info = InternalRunnerTestInfo(
            type = "custom",
            parse_test_listing = lambda stdout: [{"name": "test"}],
            parse_test_result = lambda stdout, stderr, exit_code: [],
            listing_command = ["binary", "--list"],
        )
        "#
        ))?;

        let err = InternalRunnerTestInfo::parse_test_listing_output(&info, "x").unwrap_err();
        assert!(err.to_string().contains("missing required key"), "{}", err);
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Rust-side callback invocation: parse_test_result_output
    // ---------------------------------------------------------------------------

    #[test]
    fn test_parse_test_result_output_basic() -> buck2_error::Result<()> {
        let info = freeze_provider(indoc!(
            r#"
        def my_result(stdout, stderr, exit_code):
            if exit_code == 0:
                return [{"name": "test_all", "status": "PASS"}]
            else:
                return [{"name": "test_all", "status": "FAIL", "message": stderr}]

        exported_info = InternalRunnerTestInfo(
            type = "custom",
            parse_test_listing = lambda stdout: [],
            parse_test_result = my_result,
            listing_command = ["binary", "--list"],
        )
        "#
        ))?;

        let results = InternalRunnerTestInfo::parse_test_result_output(&info, "", "", 0)?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "test_all");
        assert_eq!(results[0].status, TestStatus::PASS);
        assert!(results[0].message.is_none());

        let results = InternalRunnerTestInfo::parse_test_result_output(&info, "", "oops", 1)?;
        assert_eq!(results[0].status, TestStatus::FAIL);
        assert_eq!(results[0].message.as_deref(), Some("oops"));
        Ok(())
    }

    #[test]
    fn test_parse_test_result_output_all_fields() -> buck2_error::Result<()> {
        let info = freeze_provider(indoc!(
            r#"
        def my_result(stdout, stderr, exit_code):
            return [{
                "name": "test_math",
                "status": "FAIL",
                "message": "Expected 3, got 4",
                "details": "full stack trace here",
                "duration": 1.5,
            }]

        exported_info = InternalRunnerTestInfo(
            type = "custom",
            parse_test_listing = lambda stdout: [],
            parse_test_result = my_result,
            listing_command = ["binary", "--list"],
        )
        "#
        ))?;

        let results = InternalRunnerTestInfo::parse_test_result_output(&info, "", "", 1)?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "test_math");
        assert_eq!(results[0].status, TestStatus::FAIL);
        assert_eq!(results[0].message.as_deref(), Some("Expected 3, got 4"));
        assert_eq!(results[0].details.as_deref(), Some("full stack trace here"));
        assert_eq!(
            results[0].duration,
            Some(std::time::Duration::from_millis(1500))
        );
        Ok(())
    }

    #[test]
    fn test_parse_test_result_output_integer_duration() -> buck2_error::Result<()> {
        let info = freeze_provider(indoc!(
            r#"
        def my_result(stdout, stderr, exit_code):
            return [{"name": "test_slow", "status": "PASS", "duration": 5}]

        exported_info = InternalRunnerTestInfo(
            type = "custom",
            parse_test_listing = lambda stdout: [],
            parse_test_result = my_result,
            listing_command = ["binary", "--list"],
        )
        "#
        ))?;

        let results = InternalRunnerTestInfo::parse_test_result_output(&info, "", "", 0)?;
        assert_eq!(results[0].duration, Some(std::time::Duration::from_secs(5)));
        Ok(())
    }

    #[test]
    fn test_parse_test_result_output_negative_duration() -> buck2_error::Result<()> {
        let info = freeze_provider(indoc!(
            r#"
        def my_result(stdout, stderr, exit_code):
            return [{"name": "test", "status": "PASS", "duration": -1.0}]

        exported_info = InternalRunnerTestInfo(
            type = "custom",
            parse_test_listing = lambda stdout: [],
            parse_test_result = my_result,
            listing_command = ["binary", "--list"],
        )
        "#
        ))?;

        let err = InternalRunnerTestInfo::parse_test_result_output(&info, "", "", 0).unwrap_err();
        assert!(err.to_string().contains("non-negative"), "{}", err);
        Ok(())
    }

    #[test]
    fn test_parse_test_result_output_invalid_status() -> buck2_error::Result<()> {
        let info = freeze_provider(indoc!(
            r#"
        def my_result(stdout, stderr, exit_code):
            return [{"name": "test", "status": "BOGUS"}]

        exported_info = InternalRunnerTestInfo(
            type = "custom",
            parse_test_listing = lambda stdout: [],
            parse_test_result = my_result,
            listing_command = ["binary", "--list"],
        )
        "#
        ))?;

        let err = InternalRunnerTestInfo::parse_test_result_output(&info, "", "", 0).unwrap_err();
        assert!(err.to_string().contains("Unknown test status"), "{}", err);
        Ok(())
    }

    #[test]
    fn test_parse_test_result_output_missing_name() -> buck2_error::Result<()> {
        let info = freeze_provider(indoc!(
            r#"
        def my_result(stdout, stderr, exit_code):
            return [{"status": "PASS"}]

        exported_info = InternalRunnerTestInfo(
            type = "custom",
            parse_test_listing = lambda stdout: [],
            parse_test_result = my_result,
            listing_command = ["binary", "--list"],
        )
        "#
        ))?;

        let err = InternalRunnerTestInfo::parse_test_result_output(&info, "", "", 0).unwrap_err();
        assert!(err.to_string().contains("missing required key"), "{}", err);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Rust-side: listing_command accessor
// ---------------------------------------------------------------------------

#[test]
fn test_listing_command_accessor() -> buck2_error::Result<()> {
    use buck2_build_api::interpreter::rule_defs::provider::builtin::external_runner_test_info::TestCommandMember;

    let info = freeze_provider(indoc!(
        r#"
        exported_info = InternalRunnerTestInfo(
            type = "gtest",
            parse_test_listing = lambda stdout: [],
            parse_test_result = lambda stdout, stderr, exit_code: [],
            listing_command = ["my_binary", "--gtest_list_tests"],
            command = ["my_binary"],
        )
        "#
    ))?;

    let listing_cmd: Vec<String> = info
        .as_ref()
        .value()
        .as_ref()
        .listing_command()
        .map(|m| match m {
            TestCommandMember::Literal(s) => s.to_owned(),
            _ => "<arg>".to_owned(),
        })
        .collect();
    assert_eq!(listing_cmd, vec!["my_binary", "--gtest_list_tests"]);

    let exec_cmd: Vec<String> = info
        .as_ref()
        .value()
        .as_ref()
        .command()
        .map(|m| match m {
            TestCommandMember::Literal(s) => s.to_owned(),
            _ => "<arg>".to_owned(),
        })
        .collect();
    assert_eq!(exec_cmd, vec!["my_binary"]);
    Ok(())
}

/// S1-PROVIDER-01: the constructors of both test infos, evaluated in a module that carries a rule
/// analysis's execution platform resolution, record a test info's execution group executor with
/// `ExecutorConfigOrigin::ExecutionGroupPlatform`, keep that origin through a field-by-field copy,
/// and give a passed `CommandExecutorConfig(...)` `ExecutorConfigOrigin::Declared`.
mod execution_group_executor_origin {
    use buck2_build_api::analysis::extra_v::AnalysisExtraValue;
    use buck2_build_api::interpreter::rule_defs::command_executor_config::ExecutorConfigOrigin;
    use buck2_build_api::interpreter::rule_defs::command_executor_config::StarlarkCommandExecutorConfig;
    use buck2_build_api::interpreter::rule_defs::command_executor_config::register_command_executor_config;
    use buck2_build_api::interpreter::rule_defs::provider::builtin::external_runner_test_info::ExternalRunnerTestInfo;
    use buck2_build_api::interpreter::rule_defs::provider::builtin::internal_runner_test_info::InternalRunnerTestInfo;
    use buck2_build_api::interpreter::rule_defs::register_rule_defs;
    use buck2_core::cells::cell_path::CellPath;
    use buck2_core::configuration::pair::ConfigurationNoExec;
    use buck2_core::execution_types::execution::ExecutionGroupResolutionPartial;
    use buck2_core::execution_types::execution::ExecutionPlatform;
    use buck2_core::execution_types::execution::ExecutionPlatformResolutionPartial;
    use buck2_core::execution_types::execution_group::NamedExecutionGroup;
    use buck2_core::execution_types::executor_config::CommandExecutorConfig;
    use buck2_core::execution_types::executor_config::Executor;
    use buck2_hash::BuckFixedSortedMap;
    use buck2_interpreter::execution_group::FrozenStarlarkExecutionGroup;
    use buck2_interpreter::execution_group::StarlarkExecutionGroup;
    use starlark::environment::GlobalsBuilder;
    use starlark::environment::Module;
    use starlark::eval::Evaluator;
    use starlark::syntax::AstModule;
    use starlark::syntax::Dialect;
    use starlark::values::Value;

    const CODE: &str = r#"
def _listing(stdout):
    return []

def _result(stdout, stderr, exit_code):
    return []

def _internal(**kwargs):
    return InternalRunnerTestInfo(
        type = "t",
        parse_test_listing = _listing,
        parse_test_result = _result,
        listing_command = ["x"],
        **kwargs
    )

def _external(**kwargs):
    return ExternalRunnerTestInfo(type = "t", command = ["x"], **kwargs)

def _cases(make):
    named = make(exec_group = TestRun)
    return {
        "recorded_default": make(exec_group = DefaultExecGroup),
        "recorded_named": named,
        "copied": make(default_executor = named.default_executor, exec_group = named.exec_group),
        "declared": make(
            default_executor = CommandExecutorConfig(local_enabled = True, remote_enabled = False),
            exec_group = TestRun,
        ),
        "no_group": make(),
    }

internal = _cases(_internal)
external = _cases(_external)
"#;

    /// The origin and executor of each case's `default_executor`, for both provider kinds.
    fn origins() -> buck2_error::Result<Vec<(String, Option<(ExecutorConfigOrigin, bool)>)>> {
        let cell_path = CellPath::testing_new("root//test/rules.bzl");
        let platform = || {
            ExecutionPlatform::legacy_execution_platform(
                CommandExecutorConfig::testing_local(),
                ConfigurationNoExec::testing_new(),
            )
        };
        // A rule that declares `DefaultExecGroup` and `TestRun`, both resolved to a local
        // execution platform.
        let resolution = ExecutionPlatformResolutionPartial::new(Some(platform()), Vec::new())
            .finalize_with_execution_groups(
                BuckFixedSortedMap::new(),
                vec![ExecutionGroupResolutionPartial {
                    group: NamedExecutionGroup::new("TestRun".to_owned(), cell_path.clone()),
                    partial: ExecutionPlatformResolutionPartial::new(Some(platform()), Vec::new()),
                    exec_dep_cfgs: BuckFixedSortedMap::new(),
                }],
                true,
            );
        Module::with_temp_heap(|module| {
            AnalysisExtraValue::get_or_init(&module)?
                .execution_platform_resolution
                .set(resolution)
                .unwrap();
            module.set(
                "DefaultExecGroup",
                module.heap().alloc(FrozenStarlarkExecutionGroup::DEFAULT),
            );
            let test_run = module
                .heap()
                .alloc(StarlarkExecutionGroup::new_unbound(cell_path));
            module.set("TestRun", test_run);
            let globals = GlobalsBuilder::standard()
                .with(register_rule_defs)
                .with(register_command_executor_config)
                .build();
            let mut eval = Evaluator::new(&module);
            test_run.export_as("TestRun", &mut eval).unwrap();
            let ast = AstModule::parse("rules.bzl", CODE.to_owned(), &Dialect::Extended).unwrap();
            eval.eval_module(ast, &globals).unwrap();
            drop(eval);

            let executor = |config: Option<&StarlarkCommandExecutorConfig>| {
                config.map(|c| (c.origin(), matches!(c.0.executor, Executor::Local(_))))
            };
            let mut origins = Vec::new();
            for kind in ["internal", "external"] {
                let cases: Value = module.get(kind).unwrap();
                for case in [
                    "recorded_default",
                    "recorded_named",
                    "copied",
                    "declared",
                    "no_group",
                ] {
                    let info = cases.at(module.heap().alloc(case), module.heap()).unwrap();
                    let default_executor = if kind == "internal" {
                        executor(
                            InternalRunnerTestInfo::from_value(info)
                                .unwrap()
                                .default_executor(),
                        )
                    } else {
                        executor(
                            ExternalRunnerTestInfo::from_value(info)
                                .unwrap()
                                .default_executor(),
                        )
                    };
                    origins.push((format!("{kind}/{case}"), default_executor));
                }
            }
            Ok(origins)
        })
    }

    #[test]
    fn an_execution_group_executor_is_recorded_with_its_origin() -> buck2_error::Result<()> {
        let recorded = Some((ExecutorConfigOrigin::ExecutionGroupPlatform, true));
        let declared = Some((ExecutorConfigOrigin::Declared, true));
        let mut expected = Vec::new();
        for kind in ["internal", "external"] {
            expected.extend([
                (format!("{kind}/recorded_default"), recorded),
                (format!("{kind}/recorded_named"), recorded),
                (format!("{kind}/copied"), recorded),
                (format!("{kind}/declared"), declared),
                (format!("{kind}/no_group"), None),
            ]);
        }
        assert_eq!(origins()?, expected);
        Ok(())
    }
}
