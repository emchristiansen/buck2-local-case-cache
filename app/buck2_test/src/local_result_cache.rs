/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Admission for exact-case local result reuse, separate from action caching.

use buck2_build_api::interpreter::rule_defs::provider::builtin::internal_runner_test_info::OwnedInternalRunnerTestInfo;
use buck2_error::buck2_error;
use buck2_test_api::data::TestStage;

#[derive(Debug, Eq, PartialEq)]
enum LocalCaseAdmission {
    Legacy,
    UnsupportedPhysicalSettlementAndAmbientInputEpoch,
}

impl LocalCaseAdmission {
    fn for_stage(stage: &TestStage, provider_requested: bool, disabled: bool) -> Self {
        match stage {
            TestStage::Listing { .. } => Self::Legacy,
            TestStage::Testing { .. } if !provider_requested || disabled => Self::Legacy,
            TestStage::Testing { .. } => Self::UnsupportedPhysicalSettlementAndAmbientInputEpoch,
        }
    }

    fn require_supported(self) -> buck2_error::Result<()> {
        match self {
            Self::Legacy => Ok(()),
            Self::UnsupportedPhysicalSettlementAndAmbientInputEpoch => Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "Local exact-case result reuse refused: original physical descendant settlement \
                 and source-qualified ambient/input-epoch eligibility are unsupported"
            )),
        }
    }
}

/// Refuse the opt-in before executor preparation, resource setup or a test child.
/// Provider permission alone cannot construct an eligible or publishable result.
pub(crate) fn require_local_case_support(
    stage: &TestStage,
    provider_requested: bool,
    disabled: bool,
) -> buck2_error::Result<()> {
    LocalCaseAdmission::for_stage(stage, provider_requested, disabled).require_supported()
}

/// Bind the direct in-process ingress to its actual frozen provider. A caller
/// cannot bypass the unsupported-context refusal using another orchestrator.
pub(crate) fn require_internal_provider_support(
    provider: &OwnedInternalRunnerTestInfo,
) -> buck2_error::Result<()> {
    if provider
        .as_ref()
        .value()
        .as_ref()
        .supports_test_execution_caching()
    {
        LocalCaseAdmission::UnsupportedPhysicalSettlementAndAmbientInputEpoch.require_supported()
    } else {
        LocalCaseAdmission::Legacy.require_supported()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn testing() -> TestStage {
        TestStage::Testing {
            suite: "suite".to_owned(),
            testcases: vec!["exact_case".to_owned()],
            variant: None,
            repeat_count: None,
        }
    }

    #[test]
    fn provider_permission_does_not_replace_unsupported_evidence() {
        assert_eq!(
            LocalCaseAdmission::for_stage(&testing(), true, false),
            LocalCaseAdmission::UnsupportedPhysicalSettlementAndAmbientInputEpoch,
        );
        assert!(require_local_case_support(&testing(), true, false).is_err());
    }

    #[test]
    fn ordinary_and_explicitly_disabled_tests_keep_the_legacy_path() {
        assert!(require_local_case_support(&testing(), false, false).is_ok());
        assert!(require_local_case_support(&testing(), true, true).is_ok());
    }

    #[test]
    fn listing_is_unchanged_even_for_an_opt_in_provider() {
        let stage = TestStage::Listing {
            suite: "suite".to_owned(),
            cacheable: true,
        };
        assert!(require_local_case_support(&stage, true, false).is_ok());
    }
}
