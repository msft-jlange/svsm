// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2022-2023 SUSE LLC
//
// Author: Joerg Roedel <jroedel@suse.de>

extern crate alloc;

use crate::address::PhysAddr;
use crate::config::SvsmConfig;
use crate::error::SvsmError;
use crate::mm::memory::write_guest_memory_map;
use crate::mm::PerCPUPageMappingGuard;
use crate::platform::{PageStateChangeOp, SVSM_PLATFORM};
use crate::sev::{pvalidate, rmp_adjust, PvalidateOp, RMPFlags};
use crate::types::{PageSize, PAGE_SIZE};
use crate::utils::{zero_mem_region, MemoryRegion};

use alloc::vec::Vec;
use bootlib::igvm_params::IgvmGuestContext;

#[derive(Clone, Debug, Default)]
pub struct GuestFwInfo {
    pub cpuid_page: Option<PhysAddr>,
    pub secrets_page: Option<PhysAddr>,
    pub caa_page: Option<PhysAddr>,
}

#[derive(Debug, Default)]
pub struct GuestFwLaunchState {
    pub caa_page: Option<PhysAddr>,
    pub vtom: u64,
    pub context: Option<IgvmGuestContext>,
}

fn validate_fw_mem_region(
    config: &SvsmConfig<'_>,
    region: MemoryRegion<PhysAddr>,
) -> Result<(), SvsmError> {
    let pstart = region.start();
    let pend = region.end();

    log::info!("Validating {:#018x}-{:#018x}", pstart, pend);

    if config.page_state_change_required() {
        SVSM_PLATFORM
            .page_state_change(region, PageSize::Regular, PageStateChangeOp::Private)
            .expect("GHCB PSC call failed to validate firmware memory");
    }

    for paddr in region.iter_pages(PageSize::Regular) {
        let guard = PerCPUPageMappingGuard::create_4k(paddr)?;
        let vaddr = guard.virt_addr();

        // SAFETY: the virtual address mapping is known to point to the guest
        // physical address range supplied by the caller.
        unsafe {
            pvalidate(vaddr, PageSize::Regular, PvalidateOp::Valid)?;

            // Make page accessible to guest VMPL
            rmp_adjust(
                vaddr,
                RMPFlags::GUEST_VMPL | RMPFlags::RWX,
                PageSize::Regular,
            )?;

            zero_mem_region(vaddr, vaddr + PAGE_SIZE);
        }
    }

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

    validate_fw_mem_region(config, region)?;
    validate_fw_memory_vec(config, next_vec)
}

fn validate_fw_memory(
    config: &SvsmConfig<'_>,
    fw_info: &GuestFwInfo,
    preval_ranges: &Option<Vec<MemoryRegion<PhysAddr>>>,
    kernel_region: &MemoryRegion<PhysAddr>,
) -> Result<(), SvsmError> {
    // Initalize vector with regions from the FW
    let mut regions = match preval_ranges {
        Some(ranges) => ranges.clone(),
        None => Vec::new(),
    };

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

fn print_guest_fw_info(fw_info: &GuestFwInfo, preval_ranges: &Option<Vec<MemoryRegion<PhysAddr>>>) {
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

    if let Some(ranges) = preval_ranges.as_ref() {
        for region in ranges {
            log::info!("  Pre-Validated Region {region:#018x}");
        }
    }
}

fn validate_fw(
    config: &SvsmConfig<'_>,
    kernel_region: &MemoryRegion<PhysAddr>,
) -> Result<(), SvsmError> {
    let flash_regions = config.get_fw_regions(kernel_region);

    for (i, region) in flash_regions.into_iter().enumerate() {
        log::info!(
            "Flash region {} at {:#018x} size {:018x}",
            i,
            region.start(),
            region.len(),
        );

        for paddr in region.iter_pages(PageSize::Regular) {
            let guard = PerCPUPageMappingGuard::create_4k(paddr)?;
            let vaddr = guard.virt_addr();
            // SAFETY: the address is known to be a guest page.
            if let Err(e) = unsafe {
                rmp_adjust(
                    vaddr,
                    RMPFlags::GUEST_VMPL | RMPFlags::RWX,
                    PageSize::Regular,
                )
            } {
                log::info!("rmpadjust failed for addr {:#018x}", vaddr);
                return Err(e);
            }
        }
    }

    Ok(())
}

pub fn prepare_fw(
    config: &SvsmConfig<'_>,
    fw_info: &GuestFwInfo,
    kernel_region: MemoryRegion<PhysAddr>,
) -> Result<(), SvsmError> {
    let preval_ranges = config.get_prevalidated_ranges();

    print_guest_fw_info(fw_info, &preval_ranges);
    validate_fw_memory(config, fw_info, &preval_ranges, &kernel_region)?;
    write_guest_memory_map(config)?;
    SVSM_PLATFORM.copy_tables_to_fw(fw_info, &kernel_region)?;
    validate_fw(config, &kernel_region)?;

    Ok(())
}
