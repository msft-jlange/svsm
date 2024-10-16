// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

use crate::address::PhysAddr;
use crate::address::VirtAddr;
use crate::cpu::cpuid::CpuidResult;
use crate::cpu::msr::write_msr;
use crate::cpu::percpu::{this_cpu, PerCpu};
use crate::cpu::IrqGuard;
use crate::error::SvsmError;
use crate::hyperv;
use crate::hyperv::HyperVMsr;
use crate::mm::alloc::allocate_pages;
use crate::mm::virt_to_phys;
use crate::utils::immut_after_init::ImmutAfterInitCell;

use core::arch::asm;

use bitfield_struct::bitfield;

#[bitfield(u64)]
struct HvHypercallInput {
    call_code: u16,
    is_fast: bool,
    #[bits(9)]
    var_hdr_size: u32,
    #[bits(5)]
    _rsvd_26_30: u32,
    is_nested: bool,
    #[bits(12)]
    element_count: u32,
    #[bits(4)]
    _rsvd_44_47: u32,
    #[bits(12)]
    start_index: u32,
    #[bits(4)]
    _rsvd_60_63: u32,
}

#[repr(u16)]
enum HvCallCode {
    HvCallStartVirtualProcessor = 0x99,
}

impl From<HvCallCode> for u16 {
    fn from(code: HvCallCode) -> Self {
        code as u16
    }
}

pub const HV_PARTITION_ID_SELF: u64 = 0xFFFF_FFFF_FFFF_FFFF;
pub const HV_INVALID_VTL: u8 = 0xFF;

static HYPERV_HYPERCALL_CODE_PAGE: ImmutAfterInitCell<VirtAddr> = ImmutAfterInitCell::uninit();

pub fn is_hyperv_hypervisor() -> bool {
    // Check if any hypervisor is present.
    if (CpuidResult::get(1, 0).ecx & 0x80000000) == 0 {
        return false;
    }

    // Get the hypervisor interface signature.
    CpuidResult::get(0x40000001, 0).eax == 0x31237648
}

pub fn hyperv_setup_hypercalls() -> Result<(), SvsmError> {
    // Allocate a page to use as the hypercall code page.
    let page = allocate_pages(1)?;
    HYPERV_HYPERCALL_CODE_PAGE
        .init(&page)
        .expect("Hypercall code page already allocated");

    // Set the guest OS ID.  The value is arbitrary.
    write_msr(HyperVMsr::GuestOSID.into(), 0xC0C0C0C0);

    // Set the hypercall code page address to the physical address of the
    // allocated page, and mark it enabled.
    let pa = virt_to_phys(page);
    write_msr(HyperVMsr::Hypercall.into(), u64::from(pa) | 1);

    Ok(())
}

fn hypercall(
    input_control: HvHypercallInput,
    input_register: PhysAddr,
    output_register: PhysAddr,
) -> u16 {
    let hypercall_va = u64::from(*HYPERV_HYPERCALL_CODE_PAGE);
    let mut output: u64;
    unsafe {
        asm!("callq *%rax",
             in("rax") hypercall_va,
             in("rcx") input_control.into_bits(),
             in("rdx") u64::from(input_register),
             in("r8") u64::from(output_register),
             lateout("rax") output,
             options(att_syntax));
    }
    (output & 0xFFFF) as u16
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct HvInputStartVirtualProcessor {
    partition_id: u64,
    vp_index: u32,
    vtl: u8,
    _rsvd: [u8; 3],
    context: hyperv::HvInitialVpContext,
}

pub fn start_vp_hypercall(cpu: &PerCpu, start_rip: u64) -> Result<(), SvsmError> {
    let context = cpu.get_initial_context(start_rip);
    let input = HvInputStartVirtualProcessor {
        partition_id: HV_PARTITION_ID_SELF,
        vtl: HV_INVALID_VTL,
        vp_index: cpu.get_apic_id(),
        context,
        ..Default::default()
    };

    let input_control =
        HvHypercallInput::new().with_call_code(HvCallCode::HvCallStartVirtualProcessor.into());

    let _status = unsafe {
        let guard = IrqGuard::new();
        let (hypercall_input, _) = this_cpu().get_hypercall_pages();
        let input_page = hypercall_input
            .vaddr
            .as_mut_ptr::<HvInputStartVirtualProcessor>();
        *input_page = input;

        let status = hypercall(input_control, hypercall_input.paddr, PhysAddr::new(0));

        drop(guard);

        status
    };

    Ok(())
}
