// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2022-2023 SUSE LLC
//
// Author: Joerg Roedel <jroedel@suse.de>

const SNP_CPUID_MAX_COUNT: usize = 64;

#[derive(Copy, Clone, Default, Debug)]
#[repr(C, packed)]
pub struct SnpCpuidFn {
    pub eax_in: u32,
    pub ecx_in: u32,
    pub xcr0_in: u64,
    pub xss_in: u64,
    pub eax_out: u32,
    pub ebx_out: u32,
    pub ecx_out: u32,
    pub edx_out: u32,
    pub reserved_1: u64,
}

///
/// `SvsmCpuidTable` is designed to have the same layout as the SNP ABI
/// definition of the CPUID table, but it is used on other platforms to
/// aggregate CPUID information.  This data may include data provided by the
/// untrusted host so it must be captured once so later references are
/// consistent.
///
#[derive(Copy, Clone, Debug)]
#[repr(C, packed)]
pub struct SvsmCpuidTable {
    pub count: u32,
    pub reserved_1: u32,
    pub reserved_2: u64,
    pub func: [SnpCpuidFn; SNP_CPUID_MAX_COUNT],
}

impl Default for SvsmCpuidTable {
    fn default() -> Self {
        SvsmCpuidTable {
            count: Default::default(),
            reserved_1: Default::default(),
            reserved_2: Default::default(),
            func: [SnpCpuidFn::default(); SNP_CPUID_MAX_COUNT],
        }
    }
}
