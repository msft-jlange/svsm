// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

pub mod hyperv;
pub mod msr;

pub use hyperv::*;
pub use msr::*;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HvSegmentRegister {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    pub attributes: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HvTableRegister {
    pub _rsvd: [u16; 3],
    pub limit: u16,
    pub base: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HvInitialVpContext {
    pub rip: u64,
    pub rsp: u64,
    pub rflags: u64,
    pub cs: HvSegmentRegister,
    pub ds: HvSegmentRegister,
    pub es: HvSegmentRegister,
    pub fs: HvSegmentRegister,
    pub gs: HvSegmentRegister,
    pub ss: HvSegmentRegister,
    pub tr: HvSegmentRegister,
    pub ldtr: HvSegmentRegister,
    pub idtr: HvTableRegister,
    pub gdtr: HvTableRegister,
    pub efer: u64,
    pub cr0: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub pat: u64,
}
