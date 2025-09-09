// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

extern crate alloc;

use crate::acpi::tables::{load_acpi_cpu_info, ACPICPUInfo, ACPITable};
use crate::address::{Address, PhysAddr, VirtAddr};
use crate::error::SvsmError;
use crate::guest_fw::{GuestFwInfo, GuestFwLaunchState};
use crate::mm::alloc::free_multiple_pages;
use crate::mm::{GuestPtr, PerCPUPageMappingGuard, PAGE_SIZE};
use crate::platform::{PageStateChangeOp, PageValidateOp, SVSM_PLATFORM};
use crate::types::PageSize;
use crate::utils::{round_to_pages, MemoryRegion};
use alloc::vec::Vec;

use bootlib::igvm_params::{IgvmGuestContext, IgvmParamBlock, IgvmParamPage};
use bootlib::kernel_launch::LOWMEM_END;
use core::mem::size_of;
use core::ops::Deref;
use core::slice;
use igvm_defs::{IgvmEnvironmentInfo, MemoryMapEntryType, IGVM_VHS_MEMORY_MAP_ENTRY};

const IGVM_MEMORY_ENTRIES_PER_PAGE: usize = PAGE_SIZE / size_of::<IGVM_VHS_MEMORY_MAP_ENTRY>();

#[derive(Clone, Debug)]
#[repr(C, align(64))]
pub struct IgvmMemoryMap {
    memory_map: [IGVM_VHS_MEMORY_MAP_ENTRY; IGVM_MEMORY_ENTRIES_PER_PAGE],
}

#[derive(Clone, Debug)]
pub struct IgvmParams<'a> {
    igvm_param_block: &'a IgvmParamBlock,
    igvm_param_page: &'a IgvmParamPage,
    igvm_memory_map: &'a IgvmMemoryMap,
    igvm_madt: &'a [u8],
    igvm_guest_context: Option<&'a IgvmGuestContext>,
}

impl IgvmParams<'_> {
    /// # Safety
    /// The caller is responsible for ensuring that the supplied virtual
    /// address corresponds to an IGVM parameter block.
    pub unsafe fn new(addr: VirtAddr) -> Result<Self, SvsmError> {
        let param_block = Self::try_aligned_ref::<IgvmParamBlock>(addr)?;
        let param_page_address = addr + param_block.param_page_offset as usize;
        let param_page = Self::try_aligned_ref::<IgvmParamPage>(param_page_address)?;
        let memory_map_address = addr + param_block.memory_map_offset as usize;
        let memory_map = Self::try_aligned_ref::<IgvmMemoryMap>(memory_map_address)?;
        let madt_address = addr + param_block.madt_offset as usize;
        // SAFETY: the parameter block correctly describes the bounds of the
        // MADT.
        let madt = unsafe {
            slice::from_raw_parts(madt_address.as_ptr::<u8>(), param_block.madt_size as usize)
        };
        let guest_context = if param_block.guest_context_offset != 0 {
            let offset = usize::try_from(param_block.guest_context_offset).unwrap();
            Some(Self::try_aligned_ref::<IgvmGuestContext>(addr + offset)?)
        } else {
            None
        };

        Ok(Self {
            igvm_param_block: param_block,
            igvm_param_page: param_page,
            igvm_memory_map: memory_map,
            igvm_madt: madt,
            igvm_guest_context: guest_context,
        })
    }

    fn try_aligned_ref<'a, T>(addr: VirtAddr) -> Result<&'a T, SvsmError> {
        // SAFETY: we trust the caller to provide an address pointing to valid
        // memory which is not mutably aliased.
        unsafe { addr.aligned_ref::<T>().ok_or(SvsmError::Firmware) }
    }

    pub fn size(&self) -> usize {
        // Calculate the total size of the parameter area.  The
        // parameter area always begins at the kernel base
        // address.
        self.igvm_param_block.param_area_size.try_into().unwrap()
    }

    pub fn find_kernel_region(&self) -> Result<MemoryRegion<PhysAddr>, SvsmError> {
        let kernel_base = PhysAddr::from(self.igvm_param_block.kernel_base);
        let mut kernel_size = self.igvm_param_block.kernel_min_size;

        // Check the untrusted hypervisor-provided memory map to see if the size of the kernel
        // should be adjusted. The base location and mimimum and maximum size specified by the
        // measured igvm_param_block are still respected to ensure a malicious memory map cannot
        // cause the SVSM kernel to overlap anything important or be so small it causes weird
        // failures. But if the hypervisor gives a memory map entry of type HIDDEN that starts at
        // kernel_start, use the size of that entry as a guide. This allows the hypervisor to
        // adjust the size of the SVSM kernel to what it expects will be needed based on the
        // machine shape.
        if let Some(memory_map_region) = self.igvm_memory_map.memory_map.iter().find(|region| {
            region.entry_type == MemoryMapEntryType::HIDDEN
                && region.starting_gpa_page_number.try_into() == Ok(kernel_base.pfn())
        }) {
            let region_size_bytes = memory_map_region
                .number_of_pages
                .try_into()
                .unwrap_or(u32::MAX)
                .saturating_mul(PAGE_SIZE as u32);
            kernel_size = region_size_bytes.clamp(
                self.igvm_param_block.kernel_min_size,
                self.igvm_param_block.kernel_max_size,
            );
        }
        Ok(MemoryRegion::<PhysAddr>::new(
            kernel_base,
            kernel_size.try_into().unwrap(),
        ))
    }

    pub fn reserved_kernel_area_size(&self) -> usize {
        self.igvm_param_block
            .kernel_reserved_size
            .try_into()
            .unwrap()
    }

    pub fn page_state_change_required(&self) -> bool {
        let environment_info = IgvmEnvironmentInfo::from(self.igvm_param_page.environment_info);
        environment_info.memory_is_shared()
    }

    pub fn get_memory_regions(&self) -> Result<Vec<MemoryRegion<PhysAddr>>, SvsmError> {
        // Count the number of memory entries present.  They must be
        // non-overlapping and strictly increasing.
        let mut number_of_entries = 0;
        let mut next_page_number = 0;
        for entry in self.igvm_memory_map.memory_map.iter() {
            if entry.number_of_pages == 0 {
                break;
            }
            if entry.starting_gpa_page_number < next_page_number {
                return Err(SvsmError::Firmware);
            }
            let next_supplied_page_number = entry.starting_gpa_page_number + entry.number_of_pages;
            if next_supplied_page_number < next_page_number {
                return Err(SvsmError::Firmware);
            }
            next_page_number = next_supplied_page_number;
            number_of_entries += 1;
        }

        // Now loop over the supplied entires and add a region for each
        // known type.
        let mut regions: Vec<MemoryRegion<PhysAddr>> = Vec::new();
        for entry in self
            .igvm_memory_map
            .memory_map
            .iter()
            .take(number_of_entries)
        {
            if entry.entry_type == MemoryMapEntryType::MEMORY {
                let starting_page: usize = entry.starting_gpa_page_number.try_into().unwrap();
                let number_of_pages: usize = entry.number_of_pages.try_into().unwrap();
                regions.push(MemoryRegion::new(
                    PhysAddr::new(starting_page * PAGE_SIZE),
                    number_of_pages * PAGE_SIZE,
                ));
            }
        }

        Ok(regions)
    }

    pub fn write_guest_memory_map(&self, map: &[MemoryRegion<PhysAddr>]) -> Result<(), SvsmError> {
        // If the parameters do not include a guest memory map area, then no
        // work is required.
        let fw_info = &self.igvm_param_block.firmware;
        if fw_info.memory_map_size == 0 {
            return Ok(());
        }

        // Map the guest memory map area into the address space.
        let mem_map_gpa = PhysAddr::from(fw_info.memory_map_address as u64);
        let mem_map_region = MemoryRegion::new(mem_map_gpa, fw_info.memory_map_size as usize);
        log::info!(
            "Filling guest IGVM memory map at {:#018x} size {:#018x}",
            mem_map_region.start(),
            mem_map_region.len(),
        );

        let mem_map_mapping =
            PerCPUPageMappingGuard::create(mem_map_region.start(), mem_map_region.end(), 0)?;
        let mem_map_va = mem_map_mapping.virt_addr();

        if self.igvm_param_block.firmware.memory_map_prevalidated == 0 {
            // The guest expects the pages in the memory map to be treated like
            // host-provided IGVM parameters, which requires the pages to be
            // validated.  Since the memory was not declared as part of the
            // guest firmware image, the pages must be validated here.
            if self.page_state_change_required() {
                SVSM_PLATFORM.page_state_change(
                    mem_map_region,
                    PageSize::Regular,
                    PageStateChangeOp::Private,
                )?;
            }

            let mem_map_va_region = MemoryRegion::new(mem_map_va, mem_map_region.len());
            // SAFETY: the virtual address region was created above to map the
            // specified physical address range and is therefore safe.
            unsafe {
                SVSM_PLATFORM
                    .validate_virtual_page_range(mem_map_va_region, PageValidateOp::Validate)?;
            }
        }

        // Calculate the maximum number of entries that can be inserted.
        let max_entries = fw_info.memory_map_size as usize / size_of::<IGVM_VHS_MEMORY_MAP_ENTRY>();
        // Return an error if an overflow occurs.
        if map.len() > max_entries {
            log::warn!(
                "Too many IGVM memory map entries ({}), max is {}",
                map.len(),
                max_entries
            );
            return Err(SvsmError::Firmware);
        }

        // Generate a guest pointer range to hold the memory map.
        let mem_map = GuestPtr::new(mem_map_va);

        for (i, entry) in map.iter().enumerate() {
            // SAFETY: mem_map_va points to newly mapped memory, whose physical
            // address is defined in the IGVM config.
            unsafe {
                mem_map
                    .offset(i as isize)
                    .write(IGVM_VHS_MEMORY_MAP_ENTRY {
                        starting_gpa_page_number: u64::from(entry.start()) / PAGE_SIZE as u64,
                        number_of_pages: (entry.len() / PAGE_SIZE) as u64,
                        entry_type: MemoryMapEntryType::default(),
                        flags: 0,
                        reserved: 0,
                    })?;
            }
        }

        // Write a zero page count into the last entry to terminate the list.
        let index = map.len();
        if index < max_entries {
            // SAFETY: mem_map_va points to newly mapped memory, whose physical
            // address is defined in the IGVM config.
            unsafe {
                mem_map
                    .offset(index as isize)
                    .write(IGVM_VHS_MEMORY_MAP_ENTRY {
                        starting_gpa_page_number: 0,
                        number_of_pages: 0,
                        entry_type: MemoryMapEntryType::default(),
                        flags: 0,
                        reserved: 0,
                    })?;
            }
        }

        Ok(())
    }

    pub fn load_cpu_info(&self) -> Result<Vec<ACPICPUInfo>, SvsmError> {
        ACPITable::new(self.igvm_madt).and_then(|t| load_acpi_cpu_info(&t))
    }

    pub fn should_launch_fw(&self) -> bool {
        self.igvm_param_block.firmware.size != 0
    }

    pub fn debug_serial_port(&self) -> u16 {
        self.igvm_param_block.debug_serial_port
    }

    pub fn get_guest_fw_info(&self) -> (GuestFwInfo, GuestFwLaunchState) {
        let mut fw_info = GuestFwInfo::default();
        let mut launch_state = GuestFwLaunchState::default();

        if self.igvm_param_block.firmware.caa_page != 0 {
            fw_info.caa_page = Some(PhysAddr::new(
                self.igvm_param_block.firmware.caa_page.try_into().unwrap(),
            ));
            launch_state.caa_page = fw_info.caa_page;
        }

        if self.igvm_param_block.firmware.secrets_page != 0 {
            fw_info.secrets_page = Some(PhysAddr::new(
                self.igvm_param_block
                    .firmware
                    .secrets_page
                    .try_into()
                    .unwrap(),
            ));
        }

        if self.igvm_param_block.firmware.cpuid_page != 0 {
            fw_info.cpuid_page = Some(PhysAddr::new(
                self.igvm_param_block
                    .firmware
                    .cpuid_page
                    .try_into()
                    .unwrap(),
            ));
        }

        if let Some(guest_context) = self.igvm_guest_context {
            launch_state.context = Some(*guest_context);
        }

        launch_state.vtom = self.igvm_param_block.vtom;

        (fw_info, launch_state)
    }

    pub fn get_prevalidated_ranges(&self) -> Option<Vec<MemoryRegion<PhysAddr>>> {
        let preval_count = self.igvm_param_block.firmware.prevalidated_count as usize;
        if preval_count != 0 {
            let mut ranges = Vec::<MemoryRegion<PhysAddr>>::new();

            for preval in self
                .igvm_param_block
                .firmware
                .prevalidated
                .iter()
                .take(preval_count)
            {
                let base = PhysAddr::from(preval.base as usize);
                ranges.push(MemoryRegion::new(base, preval.size as usize));
            }

            Some(ranges)
        } else {
            None
        }
    }

    pub fn get_fw_regions(&self) -> Vec<MemoryRegion<PhysAddr>> {
        assert!(self.should_launch_fw());

        let mut regions = Vec::new();
        let fw_info = &self.igvm_param_block.firmware;

        if fw_info.in_low_memory != 0 {
            // Add the lowmem region to the firmware region list so
            // permissions can be granted to the guest VMPL for that range.
            regions.push(MemoryRegion::from_addresses(
                PhysAddr::from(0u64),
                PhysAddr::from(u64::from(LOWMEM_END)),
            ));
        }

        let fw_region =
            MemoryRegion::new(PhysAddr::new(fw_info.start as usize), fw_info.size as usize);
        regions.push(fw_region);

        // If this firmware expects an IGVM memory map but the IGVM memory
        // map is not within any of the firmware GPA ranges, then add the IGVM
        // memory map to the set of firmware regions.
        if fw_info.memory_map_size != 0 {
            let map_region = MemoryRegion::new(
                PhysAddr::from(fw_info.memory_map_address as u64),
                fw_info.memory_map_size as usize,
            );
            // Scan all the firmware regions to determine whether any of them
            // contain the memory map.  If not, the memory map should be added
            // separately.
            if !regions.iter().any(|region| {
                if region.contains_region(&map_region) {
                    true
                } else {
                    // A properly constructed image should never place the
                    // memory map partially inside and partially outside of the
                    // firmware region, so reject such a case to prevent
                    // overlapping firmware regions.
                    assert!(!region.overlap(&map_region));
                    false
                }
            }) {
                regions.push(map_region);
            }
        }

        regions
    }

    pub fn fw_in_low_memory(&self) -> bool {
        self.igvm_param_block.firmware.in_low_memory != 0
    }

    pub fn get_vtom(&self) -> u64 {
        self.igvm_param_block.vtom
    }

    pub fn use_alternate_injection(&self) -> bool {
        self.igvm_param_block.use_alternate_injection != 0
    }

    pub fn suppress_svsm_interrupts_on_snp(&self) -> bool {
        self.igvm_param_block.suppress_svsm_interrupts_on_snp != 0
    }

    pub fn has_qemu_testdev(&self) -> bool {
        self.igvm_param_block.has_qemu_testdev != 0
    }

    pub fn has_fw_cfg_port(&self) -> bool {
        self.igvm_param_block.has_fw_cfg_port != 0
    }

    pub fn has_test_iorequests(&self) -> bool {
        self.igvm_param_block.has_test_iorequests != 0
    }
}

/// `IgvmBox` is a `Box`-type object that tracks the allocation lifetime of the
/// IGVM parameters.  This is implemented separately from `PageBox` because
/// unlike normal heap allocations, the IGVM parameters are allocated as a
/// sequence of single pages, and thus cannot be freed in a single operation.
#[derive(Debug)]
pub struct IgvmBox<'a> {
    vaddr: VirtAddr,
    igvm_params: IgvmParams<'a>,
}

impl IgvmBox<'_> {
    /// # Safety
    /// The caller is responsible for ensuring that the supplied virtual
    /// address corresponds to an IGVM parameter block.
    pub unsafe fn new(vaddr: VirtAddr) -> Result<Self, SvsmError> {
        // SAFETY: the caller guarantees the correctness of the virtual
        // address.
        unsafe { IgvmParams::new(vaddr) }.map(|igvm_params| Self { vaddr, igvm_params })
    }
}

impl<'a> Deref for IgvmBox<'a> {
    type Target = IgvmParams<'a>;
    fn deref(&self) -> &IgvmParams<'a> {
        &self.igvm_params
    }
}

impl Drop for IgvmBox<'_> {
    fn drop(&mut self) {
        let page_count = round_to_pages(self.igvm_params.igvm_param_block.param_area_size as usize);
        free_multiple_pages(self.vaddr, page_count);
    }
}
