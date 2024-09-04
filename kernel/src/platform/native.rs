// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange <jlange@microsoft.com>

use crate::address::{PhysAddr, VirtAddr};
use crate::console::init_console;
use crate::cpu::cpuid::CpuidResult;
use crate::cpu::msr::write_msr;
use crate::cpu::percpu::PerCpu;
use crate::error::SvsmError;
use crate::io::IOPort;
use crate::platform::{PageEncryptionMasks, PageStateChangeOp, PlatformEnvironment, SvsmPlatform};
use crate::serial::SerialPort;
use crate::svsm_console::NativeIOPort;
use crate::types::PageSize;
use crate::utils::immut_after_init::ImmutAfterInitCell;
use crate::utils::MemoryRegion;

static CONSOLE_IO: NativeIOPort = NativeIOPort::new();
static CONSOLE_SERIAL: ImmutAfterInitCell<SerialPort<'_>> = ImmutAfterInitCell::uninit();

const APIC_MSR_ICR: u32 = 0x830;

#[derive(Clone, Copy, Debug)]
pub struct NativePlatform {}

impl NativePlatform {
    pub fn new<T: PlatformEnvironment>(_env: &T) -> Self {
        Self {}
    }
}

impl SvsmPlatform for NativePlatform {
    fn env_setup(&mut self, debug_serial_port: u16, _vtom: usize) -> Result<(), SvsmError> {
        // In the native platform, console output does not require the use of
        // any platform services, so it can be initialized immediately.
        CONSOLE_SERIAL
            .init(&SerialPort::new(&CONSOLE_IO, debug_serial_port))
            .map_err(|_| SvsmError::Console)?;
        (*CONSOLE_SERIAL).init();
        init_console(&*CONSOLE_SERIAL).map_err(|_| SvsmError::Console)
    }

    fn env_setup_late(&mut self, _debug_serial_port: u16) -> Result<(), SvsmError> {
        Ok(())
    }

    fn env_setup_svsm(&self) -> Result<(), SvsmError> {
        Ok(())
    }

    fn setup_percpu(&self, _cpu: &PerCpu) -> Result<(), SvsmError> {
        Ok(())
    }

    fn setup_percpu_current(&self, _cpu: &PerCpu) -> Result<(), SvsmError> {
        Ok(())
    }

    fn get_page_encryption_masks(&self) -> PageEncryptionMasks {
        // Find physical address size.
        let res = CpuidResult::get(0x80000008, 0);
        PageEncryptionMasks {
            private_pte_mask: 0,
            shared_pte_mask: 0,
            addr_mask_width: 64,
            phys_addr_sizes: res.eax,
        }
    }

    fn cpuid(&self, eax: u32) -> Option<CpuidResult> {
        Some(CpuidResult::get(eax, 0))
    }

    fn setup_guest_host_comm(&mut self, _cpu: &PerCpu, _is_bsp: bool) {}

    fn get_io_port(&self) -> &'static dyn IOPort {
        &CONSOLE_IO
    }

    fn page_state_change(
        &self,
        _region: MemoryRegion<PhysAddr>,
        _size: PageSize,
        _op: PageStateChangeOp,
    ) -> Result<(), SvsmError> {
        Ok(())
    }

    /// Marks a range of pages as valid for use as private pages.
    fn validate_page_range(&self, _region: MemoryRegion<VirtAddr>) -> Result<(), SvsmError> {
        Ok(())
    }

    /// Marks a range of pages as invalid for use as private pages.
    fn invalidate_page_range(&self, _region: MemoryRegion<VirtAddr>) -> Result<(), SvsmError> {
        Ok(())
    }

    fn configure_alternate_injection(&mut self, _alt_inj_requested: bool) -> Result<(), SvsmError> {
        Ok(())
    }

    fn change_apic_registration_state(&self, _incr: bool) -> Result<bool, SvsmError> {
        Err(SvsmError::NotSupported)
    }

    fn query_apic_registration_state(&self) -> bool {
        false
    }

    fn post_irq(&self, icr: u64) -> Result<(), SvsmError> {
        write_msr(APIC_MSR_ICR, icr);
        Ok(())
    }

    fn eoi(&self) {
        todo!();
    }

    fn start_cpu(&self, _cpu: &PerCpu, _start_rip: u64) -> Result<(), SvsmError> {
        todo!();
    }
}
