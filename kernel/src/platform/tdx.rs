// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange <jlange@microsoft.com>

use crate::cpu::cpuid::populate_cpuid_table;
use crate::platform::SvsmPlatform;

use cpuarch::cpuid::SvsmCpuidTable;

#[derive(Clone, Copy, Debug)]
pub struct TdxPlatform {}

impl TdxPlatform {
    pub fn new() -> Self {
        Self {}
    }
}

impl SvsmPlatform for TdxPlatform {
    fn env_setup(&mut self) {}
    fn use_shared_gpa_bit(&self) -> bool {
        true
    }
    fn prepare_cpuid_table(&self, cpuid_page: &'static mut SvsmCpuidTable) {
        populate_cpuid_table(cpuid_page);
    }
}
