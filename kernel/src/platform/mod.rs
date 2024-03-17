// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange <jlange@microsoft.com>

use crate::platform::snp::SnpPlatform;
use crate::platform::tdx::TdxPlatform;

use bootlib::platform::SvsmPlatformType;
use cpuarch::cpuid::SvsmCpuidTable;

pub mod snp;
pub mod tdx;

pub trait SvsmPlatform {
    fn env_setup(&mut self);
    fn use_shared_gpa_bit(&self) -> bool;
    fn prepare_cpuid_table(&self, cpuid_page: &'static mut SvsmCpuidTable);
}

//FIXME - remove Copy trait
#[derive(Clone, Copy, Debug)]
pub enum SvsmPlatformCell {
    Snp(SnpPlatform),
    Tdx(TdxPlatform),
}

impl SvsmPlatformCell {
    pub fn new(platform_type: SvsmPlatformType) -> Self {
        match platform_type {
            SvsmPlatformType::Snp => SvsmPlatformCell::Snp(SnpPlatform::new()),
            SvsmPlatformType::Tdx => SvsmPlatformCell::Tdx(TdxPlatform::new()),
        }
    }

    pub fn as_mut_dyn_ref(&mut self) -> &mut dyn SvsmPlatform {
        match self {
            SvsmPlatformCell::Snp(platform) => platform,
            SvsmPlatformCell::Tdx(platform) => platform,
        }
    }
}
