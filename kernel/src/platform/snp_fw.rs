// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2022-2023 SUSE LLC
//
// Author: Joerg Roedel <jroedel@suse.de>

use crate::address::PhysAddr;
use crate::cpu::cpuid::copy_cpuid_table_to;
use crate::cpu::efer::EFERFlags;
use crate::cpu::percpu::{current_ghcb, this_cpu, this_cpu_shared};
use crate::error::SvsmError;
use crate::guest_fw::{GuestFwInfo, GuestFwLaunchState};
use crate::mm::PerCPUPageMappingGuard;
use crate::sev::secrets_page;
use crate::types::{GUEST_VMPL, PAGE_SIZE};
use crate::utils::{zero_mem_region, MemoryRegion};

use cpuarch::vmsa::VMSA;

fn copy_cpuid_table_to_fw(fw_addr: PhysAddr) -> Result<(), SvsmError> {
    let guard = PerCPUPageMappingGuard::create_4k(fw_addr)?;

    // SAFETY: this is called from CPU 0, so the underlying physical address
    // is not being aliased. We are mapping a full page, which is 4K-aligned,
    // and is enough for SnpCpuidTable.
    unsafe {
        copy_cpuid_table_to(guard.virt_addr());
    }

    Ok(())
}

fn copy_secrets_page_to_fw(
    fw_addr: PhysAddr,
    caa_addr: PhysAddr,
    kernel_region: &MemoryRegion<PhysAddr>,
) -> Result<(), SvsmError> {
    let guard = PerCPUPageMappingGuard::create_4k(fw_addr)?;
    let start = guard.virt_addr();

    // Zero target
    // SAFETY: we trust PerCPUPageMappingGuard::create_4k() to return a
    // valid pointer to a correctly mapped region of size PAGE_SIZE.
    unsafe {
        zero_mem_region(start, start + PAGE_SIZE);
    }

    // Copy secrets page
    let mut fw_secrets_page = secrets_page().unwrap().copy_for_vmpl(GUEST_VMPL);

    fw_secrets_page.set_svsm_data(
        kernel_region.start().into(),
        kernel_region.len().try_into().unwrap(),
        u64::from(caa_addr),
    );

    // SAFETY: start points to a new allocated and zeroed page.
    unsafe {
        fw_secrets_page.copy_to(start);
    }

    Ok(())
}

fn zero_caa_page(fw_addr: PhysAddr) -> Result<(), SvsmError> {
    let guard = PerCPUPageMappingGuard::create_4k(fw_addr)?;
    let vaddr = guard.virt_addr();

    // SAFETY: we trust PerCPUPageMappingGuard::create_4k() to return a
    // valid pointer to a correctly mapped region of size PAGE_SIZE.
    unsafe {
        zero_mem_region(vaddr, vaddr + PAGE_SIZE);
    }

    Ok(())
}

pub fn copy_tables_to_fw(
    fw_info: &GuestFwInfo,
    kernel_region: &MemoryRegion<PhysAddr>,
) -> Result<(), SvsmError> {
    if let Some(addr) = fw_info.cpuid_page {
        copy_cpuid_table_to_fw(addr)?;
    }

    let secrets_page = fw_info.secrets_page.ok_or(SvsmError::MissingSecrets)?;
    let caa_page = fw_info.caa_page.ok_or(SvsmError::MissingCAA)?;

    copy_secrets_page_to_fw(secrets_page, caa_page, kernel_region)?;

    zero_caa_page(caa_page)?;

    Ok(())
}

pub fn prepare_fw_launch(launch_state: &GuestFwLaunchState) -> Result<(), SvsmError> {
    if let Some(caa) = launch_state.caa_page {
        this_cpu_shared().update_guest_caa(caa);
    }

    this_cpu().alloc_guest_vmsa()?;
    this_cpu().update_guest_mappings()?;

    Ok(())
}

pub fn initialize_guest_vmsa(
    vmsa: &mut VMSA,
    launch_state: &GuestFwLaunchState,
) -> Result<(), SvsmError> {
    let Some(ref guest_context) = launch_state.context else {
        return Ok(());
    };

    // Copy the specified registers into the VMSA.
    vmsa.cr0 = guest_context.cr0;
    vmsa.cr3 = guest_context.cr3;
    vmsa.cr4 = guest_context.cr4;
    vmsa.efer = guest_context.efer;
    vmsa.rip = guest_context.rip;
    vmsa.rax = guest_context.rax;
    vmsa.rcx = guest_context.rcx;
    vmsa.rdx = guest_context.rdx;
    vmsa.rbx = guest_context.rbx;
    vmsa.rsp = guest_context.rsp;
    vmsa.rbp = guest_context.rbp;
    vmsa.rsi = guest_context.rsi;
    vmsa.rdi = guest_context.rdi;
    vmsa.r8 = guest_context.r8;
    vmsa.r9 = guest_context.r9;
    vmsa.r10 = guest_context.r10;
    vmsa.r11 = guest_context.r11;
    vmsa.r12 = guest_context.r12;
    vmsa.r13 = guest_context.r13;
    vmsa.r14 = guest_context.r14;
    vmsa.r15 = guest_context.r15;
    vmsa.gdt.base = guest_context.gdt_base;
    vmsa.gdt.limit = guest_context.gdt_limit;

    // If a non-zero code selector is specified, then set the code segment
    // attributes based on EFER.LMA.
    if guest_context.code_selector != 0 {
        vmsa.cs.selector = guest_context.code_selector;
        let efer_lma = EFERFlags::LMA;
        if (vmsa.efer & efer_lma.bits()) != 0 {
            vmsa.cs.flags = 0xA9B;
        } else {
            vmsa.cs.flags = 0xC9B;
            vmsa.cs.limit = 0xFFFFFFFF;
        }
    }

    let efer_svme = EFERFlags::SVME;
    vmsa.efer &= !efer_svme.bits();

    // If a non-zero data selector is specified, then modify the data segment
    // attributes to be compatible with protected mode.
    if guest_context.data_selector != 0 {
        vmsa.ds.selector = guest_context.data_selector;
        vmsa.ds.flags = 0xA93;
        vmsa.ds.limit = 0xFFFFFFFF;
        vmsa.ss = vmsa.ds;
        vmsa.es = vmsa.ds;
        vmsa.fs = vmsa.ds;
        vmsa.gs = vmsa.ds;
    }

    // Configure vTOM if requested.
    if launch_state.vtom != 0 {
        vmsa.vtom = launch_state.vtom;
        vmsa.sev_features |= 2; // VTOM feature
    }

    Ok(())
}

pub fn launch_fw(launch_state: &GuestFwLaunchState) -> Result<(), SvsmError> {
    prepare_fw_launch(launch_state)?;

    let cpu = this_cpu();
    let mut vmsa_ref = cpu.guest_vmsa_ref();
    let vmsa_pa = vmsa_ref.vmsa_phys().unwrap();
    let vmsa = vmsa_ref.vmsa();

    initialize_guest_vmsa(vmsa, launch_state)?;

    log::info!("VMSA PA: {:#x}", vmsa_pa);

    let sev_features = vmsa.sev_features;

    log::info!("Launching Firmware");
    current_ghcb().register_guest_vmsa(vmsa_pa, 0, GUEST_VMPL as u64, sev_features)?;

    Ok(())
}
