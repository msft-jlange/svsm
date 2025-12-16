// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange <jlange@microsoft.com>

use crate::elf::ElfSizes;
use crate::page_tables::{KernelPageTables, PteType};
use crate::BootImageHost;

use igvm_defs::PAGE_SIZE_4K;

#[derive(Debug)]
pub struct KernelPageHeap {
    pub virt_base: u64,
    pub phys_base: u64,
    pub page_count: u64,
    pub usable_pages: u64,
    pub next_free: u64,
}

impl KernelPageHeap {
    fn create(phys_base: u64, page_count: u64, virt_base: u64, vmsa_reserve: bool) -> Self {
        // If the VMSA will reside in the kernel heap area, then reserve one
        // page from the total heap size.
        let reserve_pages = vmsa_reserve as u64;
        Self {
            virt_base,
            phys_base,
            page_count,
            usable_pages: page_count - reserve_pages,
            next_free: 0,
        }
    }

    pub fn virt_base(&self) -> u64 {
        self.virt_base
    }

    pub fn phys_base(&self) -> u64 {
        self.phys_base
    }

    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    pub fn next_free(&self) -> u64 {
        self.next_free
    }

    /// Allocates pages out of the heap memory area.  The returned value is a
    /// tuple containing the physical address and virtual address of the
    /// allocation.
    pub fn allocate_pages(&mut self, page_count: u64) -> Result<(u64, u64), ()> {
        // Allocation can only be successful if the heap is large enough to
        // accommodate the allocation request.
        if self.next_free + page_count <= self.usable_pages {
            let offset = self.next_free * PAGE_SIZE_4K;
            let (phys, virt) = (self.phys_base + offset, self.virt_base + offset);
            self.next_free += page_count;
            Ok((phys, virt))
        } else {
            Err(())
        }
    }

    pub fn map<'a, H: BootImageHost<'a>>(
        &self,
        page_tables: &mut KernelPageTables,
        host: &H,
    ) -> Result<(), ()> {
        page_tables.map_range(
            self.virt_base,
            self.page_count * PAGE_SIZE_4K,
            self.phys_base,
            PteType::RwData,
            host,
        )
    }
}

/// Creates a structure used to describe the kernel heap and the associated
/// allocations that are made as the boot image is prepared.  The kernel heap
/// begins immediately after the kernel image and extends to the top of the
/// allocated kernel memory region.
pub fn create_kernel_heap<'a, H: BootImageHost<'a>>(
    kernel_phys_base: u64,
    kernel_page_count: u64,
    kernel_elf_sizes: &ElfSizes,
    virtual_reserve: u64,
    vmsa_reserve: bool,
    host: &H,
) -> Result<KernelPageHeap, ()> {
    // Calculate the base and size of the heap by subtracting the kernel
    // region.
    let kernel_size = kernel_elf_sizes.phys_size * PAGE_SIZE_4K;
    let heap_pstart = kernel_phys_base + kernel_size;

    // Compute size.
    let heap_size = kernel_page_count
        .checked_sub(kernel_elf_sizes.phys_size)
        .ok_or(())
        .inspect_err(|_| host.display_error("Insufficient physical space for kernel image"))?;

    // Calculate the base virtual address of the heap.
    let heap_vstart = kernel_elf_sizes.virt_base + kernel_elf_sizes.virt_len + virtual_reserve;
    let heap = KernelPageHeap::create(heap_pstart, heap_size, heap_vstart, vmsa_reserve);

    Ok(heap)
}
