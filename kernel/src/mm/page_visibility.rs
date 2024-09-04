// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

use crate::address::VirtAddr;
use crate::cpu::flush_tlb_global_sync;
use crate::cpu::percpu::this_cpu;
use crate::error::SvsmError;
use crate::mm::validate::{
    valid_bitmap_clear_valid_4k, valid_bitmap_set_valid_4k, valid_bitmap_valid_addr,
};
use crate::mm::virt_to_phys;
use crate::platform::{PageStateChangeOp, SVSM_PLATFORM};
use crate::types::{PageSize, PAGE_SIZE};
use crate::utils::MemoryRegion;

/// Makes a virtual page shared by revoking its validation, updating the
/// page state, and modifying the page tables accordingly.
///
/// # Arguments
///
/// * `vaddr` - The virtual address of the page to be made shared.
pub fn make_page_shared(vaddr: VirtAddr) -> Result<(), SvsmError> {
    let platform = SVSM_PLATFORM.get();

    // Revoke page validation before changing page state.
    let paddr = virt_to_phys(vaddr);
    platform.invalidate_page_range(MemoryRegion::new(paddr, PAGE_SIZE))?;
    if valid_bitmap_valid_addr(paddr) {
        valid_bitmap_clear_valid_4k(paddr);
    }

    // Ask the hypervisor to make the page shared.
    platform.page_state_change(
        MemoryRegion::new(paddr, PAGE_SIZE),
        PageSize::Regular,
        PageStateChangeOp::Shared,
    )?;

    // Update the page tables to map the page as shared.
    this_cpu()
        .get_pgtable()
        .set_shared_4k(vaddr)
        .expect("Failed to remap shared page in page tables");
    flush_tlb_global_sync();

    Ok(())
}

/// Makes a virtual page private by updating the page tables, modifying the
/// page state, and revalidating the page.
///
/// # Arguments
///
/// * `vaddr` - The virtual address of the page to be made private.
pub fn make_page_private(vaddr: VirtAddr) -> Result<(), SvsmError> {
    // Update the page tables to map the page as private.
    this_cpu().get_pgtable().set_encrypted_4k(vaddr)?;
    flush_tlb_global_sync();

    let platform = SVSM_PLATFORM.get();

    // Ask the hypervisor to make the page private.
    let paddr = virt_to_phys(vaddr);
    platform.page_state_change(
        MemoryRegion::new(paddr, PAGE_SIZE),
        PageSize::Regular,
        PageStateChangeOp::Private,
    )?;

    // Revoke page validation before changing page state.
    platform.validate_page_range(MemoryRegion::new(paddr, PAGE_SIZE))?;
    if valid_bitmap_valid_addr(paddr) {
        valid_bitmap_set_valid_4k(paddr);
    }

    Ok(())
}
