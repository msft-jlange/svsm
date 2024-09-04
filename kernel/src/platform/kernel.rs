// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange <jlange@microsoft.com>

use crate::address::{PhysAddr, VirtAddr};
use crate::error::SvsmError;
use crate::mm::PerCPUPageMappingGuard;
use crate::platform::{MappingGuard, PlatformEnvironment};

#[derive(Clone, Copy, Debug)]
pub struct KernelEnvironment {}

pub static KERNEL_ENVIRONMENT: KernelEnvironment = KernelEnvironment::new();

struct KernelMappingGuard {
    guard: PerCPUPageMappingGuard,
}

impl KernelMappingGuard {
    fn new(guard: PerCPUPageMappingGuard) -> Self {
        Self { guard }
    }
}

impl MappingGuard for KernelMappingGuard {
    fn virt_addr(&self) -> VirtAddr {
        self.guard.virt_addr()
    }
}

impl KernelEnvironment {
    const fn new() -> Self {
        Self {}
    }

    pub fn env() -> &'static Self {
        &KERNEL_ENVIRONMENT
    }
}

impl PlatformEnvironment for KernelEnvironment {
    fn map_phys_range(&self, paddr: PhysAddr, len: usize) -> Result<impl MappingGuard, SvsmError> {
        let guard = PerCPUPageMappingGuard::create(paddr, paddr + len, 0)?;
        Ok(KernelMappingGuard::new(guard))
    }
}
