// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2022-2023 SUSE LLC
//
// Author: Nicolai Stange <nstange@suse.de>

#![no_std]
#![cfg_attr(test_in_svsm, no_main)]
#![cfg_attr(test_in_svsm, feature(custom_test_frameworks))]
#![cfg_attr(test_in_svsm, test_runner(svsm::testing::svsm_test_runner))]
#![cfg_attr(test_in_svsm, reexport_test_harness_main = "test_main")]

// When running tests inside the SVSM:
// Build the kernel entrypoint.
#[cfg(test_in_svsm)]
#[path = "svsm.rs"]
pub mod svsm_bin;
