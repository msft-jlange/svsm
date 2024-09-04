// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange <jlange@microsoft.com>

use crate::address::{PhysAddr, VirtAddr};
use crate::error::SvsmError;
use crate::mm::phys_to_virt;
use crate::platform::{MappingGuard, PlatformEnvironment};

#[derive(Clone, Copy, Debug)]
pub struct Stage2Environment {}

static STAGE2_ENVIRONMENT: Stage2Environment = Stage2Environment::new();

struct Stage2MappingGuard {
    vaddr: VirtAddr,
}

impl Stage2MappingGuard {
    fn new(vaddr: VirtAddr) -> Self {
        Self { vaddr }
    }
}

impl MappingGuard for Stage2MappingGuard {
    fn virt_addr(&self) -> VirtAddr {
        self.vaddr
    }
}

impl Stage2Environment {
    const fn new() -> Self {
        Self {}
    }

    pub fn env() -> &'static Self {
        &STAGE2_ENVIRONMENT
    }
}

impl PlatformEnvironment for Stage2Environment {
    fn map_phys_range(&self, paddr: PhysAddr, _len: usize) -> Result<impl MappingGuard, SvsmError> {
        // In the stage2 environment, only addresses in the virt-to-phys map
        // are accessible, so simply translate the phsical address to a virtual
        // address.
        Ok(Stage2MappingGuard::new(phys_to_virt(paddr)))
    }
}
