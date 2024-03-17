// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange <jlange@microsoft.com>

use crate::platform::SvsmPlatform;
use crate::sev::sev_status_init;
use crate::sev::status::vtom_enabled;

use cpuarch::cpuid::SvsmCpuidTable;

#[derive(Clone, Copy, Debug)]
pub struct SnpPlatform {}

impl SnpPlatform {
    pub fn new() -> Self {
        Self {}
    }
}

impl SvsmPlatform for SnpPlatform {
    fn env_setup(&mut self) {
        sev_status_init();
    }

    fn use_shared_gpa_bit(&self) -> bool {
        vtom_enabled()
    }

    fn prepare_cpuid_table(&self, _cpuid_page: &'static mut SvsmCpuidTable) {
    }
}
