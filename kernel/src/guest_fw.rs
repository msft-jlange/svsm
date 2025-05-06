// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2022-2023 SUSE LLC
//
// Author: Joerg Roedel <jroedel@suse.de>

extern crate alloc;

use crate::address::PhysAddr;
use crate::config::SvsmConfig;
use crate::error::SvsmError;
use crate::mm::{GuestPtr, PerCPUPageMappingGuard};
use crate::platform::{PageStateChangeOp, PageValidateOp, SVSM_PLATFORM};
use crate::types::{PageSize, PAGE_SIZE};
use crate::utils::MemoryRegion;
use alloc::vec::Vec;
use igvm_defs::{MemoryMapEntryType, IGVM_VHS_MEMORY_MAP_ENTRY};

#[derive(Clone, Debug, Default)]
pub struct GuestFwInfo {
    pub cpuid_page: Option<PhysAddr>,
    pub secrets_page: Option<PhysAddr>,
    pub caa_page: Option<PhysAddr>,
    pub guest_mem_map: Option<MemoryRegion<PhysAddr>>,
    pub valid_mem: Vec<MemoryRegion<PhysAddr>>,
}

impl GuestFwInfo {
    pub const fn new() -> Self {
        Self {
            cpuid_page: None,
            secrets_page: None,
            caa_page: None,
            guest_mem_map: None,
            valid_mem: Vec::new(),
        }
    }

    pub fn add_valid_mem(&mut self, base: PhysAddr, len: usize) {
        self.valid_mem.push(MemoryRegion::new(base, len));
    }

    pub fn write_guest_memory_map(&self, map: &[MemoryRegion<PhysAddr>]) -> Result<(), SvsmError> {
        // If the parameters do not include a guest memory map area, then no
        // work is required.
        if let Some(mem_map_region) = self.guest_mem_map {
            // Map the guest memory map area into the address space.
            log::info!(
                "Filling guest IGVM memory map at {:#018x} size {:#018x}",
                mem_map_region.start(),
                mem_map_region.len(),
            );

            let mem_map_mapping =
                PerCPUPageMappingGuard::create(mem_map_region.start(), mem_map_region.end(), 0)?;
            let mem_map_va = mem_map_mapping.virt_addr();

            // Calculate the maximum number of entries that can be inserted.
            let max_entries = mem_map_region.len() / size_of::<IGVM_VHS_MEMORY_MAP_ENTRY>();

            // Generate a guest pointer range to hold the memory map.
            let mem_map = GuestPtr::new(mem_map_va);

            for (i, entry) in map.iter().enumerate() {
                // Return an error if an overflow occurs.
                if i >= max_entries {
                    return Err(SvsmError::Firmware);
                }

                // SAFETY: mem_map_va points to newly mapped memory, whose
                // physical address is defined in the IGVM config.
                unsafe {
                    mem_map
                        .offset(i as isize)
                        .write(IGVM_VHS_MEMORY_MAP_ENTRY {
                            starting_gpa_page_number: u64::from(entry.start()) / PAGE_SIZE as u64,
                            number_of_pages: entry.len() as u64 / PAGE_SIZE as u64,
                            entry_type: MemoryMapEntryType::default(),
                            flags: 0,
                            reserved: 0,
                        })?;
                }
            }

            // Write a zero page count into the last entry to terminate the
            // list.
            let index = map.len();
            if index < max_entries {
                // SAFETY: mem_map_va points to newly mapped memory, whose
                // physical address is defined in the IGVM config.
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
        }

        Ok(())
    }
}

fn validate_fw_mem_region(
    config: &SvsmConfig<'_>,
    region: MemoryRegion<PhysAddr>,
) -> Result<(), SvsmError> {
    let pstart = region.start();
    let pend = region.end();
    let platform = *SVSM_PLATFORM;

    log::info!("Validating {:#018x}-{:#018x}", pstart, pend);

    if config.page_state_change_required() {
        platform
            .page_state_change(region, PageSize::Regular, PageStateChangeOp::Private)
            .expect("GHCB PSC call failed to validate firmware memory");
    }

    platform.validate_physical_page_range(region, PageValidateOp::Validate, true)?;

    Ok(())
}

fn validate_fw_memory_vec(
    config: &SvsmConfig<'_>,
    regions: Vec<MemoryRegion<PhysAddr>>,
) -> Result<(), SvsmError> {
    if regions.is_empty() {
        return Ok(());
    }

    let mut next_vec = Vec::new();
    let mut region = regions[0];

    for next in regions.into_iter().skip(1) {
        if region.contiguous(&next) {
            region = region.merge(&next);
        } else {
            next_vec.push(next);
        }
    }

    if let Some(guest_mem_map) = config.guest_mem_map_region() {
        // The guest expects the pages in the memory map to be treated like
        // host provided IGVM parameters, which requires the pages to be
        // validated.  Since the memory was not declared as part of the guest
        // firmware image, the pages must be validated here.
        validate_fw_mem_region(config, guest_mem_map)?;
    }
    validate_fw_mem_region(config, region)?;
    validate_fw_memory_vec(config, next_vec)
}

fn validate_fw_memory(
    config: &SvsmConfig<'_>,
    fw_info: &GuestFwInfo,
    kernel_region: &MemoryRegion<PhysAddr>,
) -> Result<(), SvsmError> {
    // Initalize vector with regions from the FW
    let mut regions = fw_info.valid_mem.clone();

    // Add region for CPUID page if present
    if let Some(cpuid_paddr) = fw_info.cpuid_page {
        regions.push(MemoryRegion::new(cpuid_paddr, PAGE_SIZE));
    }

    // Add region for Secrets page if present
    if let Some(secrets_paddr) = fw_info.secrets_page {
        regions.push(MemoryRegion::new(secrets_paddr, PAGE_SIZE));
    }

    // Add region for CAA page if present
    if let Some(caa_paddr) = fw_info.caa_page {
        regions.push(MemoryRegion::new(caa_paddr, PAGE_SIZE));
    }

    // Sort regions by base address
    regions.sort_unstable_by_key(|a| a.start());

    for region in regions.iter() {
        if region.overlap(kernel_region) {
            log::error!("FwMeta region ovelaps with kernel");
            return Err(SvsmError::Firmware);
        }
    }

    validate_fw_memory_vec(config, regions)
}

fn validate_fw(
    config: &SvsmConfig<'_>,
    kernel_region: &MemoryRegion<PhysAddr>,
) -> Result<(), SvsmError> {
    let flash_regions = config.get_fw_regions(kernel_region);
    let platform = *SVSM_PLATFORM;

    for (i, region) in flash_regions.into_iter().enumerate() {
        log::info!(
            "Flash region {} at {:#018x} size {:018x}",
            i,
            region.start(),
            region.len(),
        );

        platform.set_guest_page_access(region, true);
    }

    Ok(())
}

fn print_fw_info(fw_info: &GuestFwInfo) {
    log::info!("FW Meta Data");

    match fw_info.cpuid_page {
        Some(addr) => log::info!("  CPUID Page   : {:#010x}", addr),
        None => log::info!("  CPUID Page   : None"),
    };

    match fw_info.secrets_page {
        Some(addr) => log::info!("  Secrets Page : {:#010x}", addr),
        None => log::info!("  Secrets Page : None"),
    };

    match fw_info.caa_page {
        Some(addr) => log::info!("  CAA Page     : {:#010x}", addr),
        None => log::info!("  CAA Page     : None"),
    };

    for region in &fw_info.valid_mem {
        log::info!("  Pre-Validated Region {region:#018x}");
    }
}

pub fn prepare_guest_fw(
    config: &SvsmConfig<'_>,
    kernel_region: MemoryRegion<PhysAddr>,
) -> Result<Option<GuestFwInfo>, SvsmError> {
    let guest_fw = config.get_fw_info();
    if let Some(fw_info) = &guest_fw {
        print_fw_info(fw_info);
        validate_fw_memory(config, fw_info, &kernel_region)?;
        validate_fw(config, &kernel_region)?;
    }

    Ok(guest_fw)
}
