// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange <jlange@microsoft.com>

use crate::address::{Address, PhysAddr};
use crate::console::init_console;
use crate::cpu::cpuid::{cpuid_table, CpuidResult};
use crate::cpu::percpu::{current_ghcb, this_cpu, PerCpu};
use crate::error::ApicError::Registration;
use crate::error::SvsmError;
use crate::io::IOPort;
use crate::mm::{PAGE_SIZE, PAGE_SIZE_2M};
use crate::platform::{PageEncryptionMasks, PageStateChangeOp, PlatformEnvironment, SvsmPlatform, MappingGuard};
use crate::serial::SerialPort;
use crate::sev::hv_doorbell::current_hv_doorbell;
use crate::sev::msr_protocol::{hypervisor_ghcb_features, verify_ghcb_version, GHCBHvFeatures};
use crate::sev::status::vtom_enabled;
use crate::sev::{
    init_hypervisor_ghcb_features, pvalidate_range, sev_status_init, sev_status_verify, PvalidateOp,
};
use crate::svsm_console::SVSMIOPort;
use crate::types::PageSize;
use crate::utils::immut_after_init::ImmutAfterInitCell;
use crate::utils::MemoryRegion;

use core::sync::atomic::{AtomicU32, Ordering};

static CONSOLE_IO: SVSMIOPort = SVSMIOPort::new();
static CONSOLE_SERIAL: ImmutAfterInitCell<SerialPort<'_>> = ImmutAfterInitCell::uninit();

static VTOM: ImmutAfterInitCell<usize> = ImmutAfterInitCell::uninit();

static APIC_EMULATION_REG_COUNT: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy, Debug)]
pub struct SnpPlatform<'a, T: PlatformEnvironment> {
    env: &'a T,
}

impl<'a, T: PlatformEnvironment> SnpPlatform<'a, T> {
    pub fn new(env: &'a T) -> Self {
        Self { env }
    }
}

fn pvalidate_page_range<T: PlatformEnvironment>(
    env: &T,
    range: MemoryRegion<PhysAddr>,
    op: PvalidateOp,
) -> Result<(), SvsmError> {
    // In the future, it is likely that this function will need to be prepared
    // to execute both PVALIDATE and RMPADJSUT over the same set of addresses,
    // so the loop is structured to anticipate that possibility.
    let mut paddr = range.start();
    let paddr_end = range.end();
    while paddr < paddr_end {
        // Check whether a 2 MB page can be attempted.
        let (mapping, len) = if paddr.is_aligned(PAGE_SIZE_2M) && paddr + PAGE_SIZE_2M <= paddr_end
        {
            let mapping = env.map_phys_range(paddr, PAGE_SIZE_2M)?;
            (mapping, PAGE_SIZE_2M)
        } else {
            let mapping = env.map_phys_range(paddr, PAGE_SIZE)?;
            (mapping, PAGE_SIZE)
        };
        pvalidate_range(MemoryRegion::new(mapping.virt_addr(), len), op)?;
        paddr = paddr + len;
    }

    Ok(())
}

impl<T: PlatformEnvironment> SvsmPlatform for SnpPlatform<'_, T> {
    fn env_setup(&mut self, _debug_serial_port: u16, vtom: usize) -> Result<(), SvsmError> {
        sev_status_init();
        VTOM.init(&vtom).map_err(|_| SvsmError::PlatformInit)?;
        Ok(())
    }

    fn env_setup_late(&mut self, debug_serial_port: u16) -> Result<(), SvsmError> {
        CONSOLE_SERIAL
            .init(&SerialPort::new(&CONSOLE_IO, debug_serial_port))
            .map_err(|_| SvsmError::Console)?;
        (*CONSOLE_SERIAL).init();
        init_console(&*CONSOLE_SERIAL).map_err(|_| SvsmError::Console)?;
        sev_status_verify();
        init_hypervisor_ghcb_features()?;
        Ok(())
    }

    fn env_setup_svsm(&self) -> Result<(), SvsmError> {
        this_cpu().configure_hv_doorbell()
    }

    fn setup_percpu(&self, cpu: &PerCpu) -> Result<(), SvsmError> {
        // Setup GHCB
        cpu.setup_ghcb()
    }

    fn setup_percpu_current(&self, cpu: &PerCpu) -> Result<(), SvsmError> {
        cpu.register_ghcb()?;
        Ok(())
    }

    fn get_page_encryption_masks(&self) -> PageEncryptionMasks {
        // Find physical address size.
        let processor_capacity =
            cpuid_table(0x80000008).expect("Can not get physical address size from CPUID table");
        if vtom_enabled() {
            let vtom = *VTOM;
            PageEncryptionMasks {
                private_pte_mask: 0,
                shared_pte_mask: vtom,
                addr_mask_width: vtom.leading_zeros(),
                phys_addr_sizes: processor_capacity.eax,
            }
        } else {
            // Find C-bit position.
            let sev_capabilities =
                cpuid_table(0x8000001f).expect("Can not get C-Bit position from CPUID table");
            let c_bit = sev_capabilities.ebx & 0x3f;
            PageEncryptionMasks {
                private_pte_mask: 1 << c_bit,
                shared_pte_mask: 0,
                addr_mask_width: c_bit,
                phys_addr_sizes: processor_capacity.eax,
            }
        }
    }

    fn cpuid(&self, eax: u32) -> Option<CpuidResult> {
        cpuid_table(eax)
    }

    fn setup_guest_host_comm(&mut self, cpu: &PerCpu, is_bsp: bool) {
        if is_bsp {
            verify_ghcb_version();
        }

        cpu.setup_ghcb().unwrap_or_else(|_| {
            if is_bsp {
                panic!("Failed to setup BSP GHCB");
            } else {
                panic!("Failed to setup AP GHCB");
            }
        });
        cpu.register_ghcb().expect("Failed to register GHCB");
    }

    fn get_io_port(&self) -> &'static dyn IOPort {
        &CONSOLE_IO
    }

    fn page_state_change(
        &self,
        region: MemoryRegion<PhysAddr>,
        size: PageSize,
        op: PageStateChangeOp,
    ) -> Result<(), SvsmError> {
        current_ghcb().page_state_change(region, size, op)
    }

    /// Marks a range of pages as valid for use as private pages.
    fn validate_page_range(&self, region: MemoryRegion<PhysAddr>) -> Result<(), SvsmError> {
        pvalidate_page_range(self.env, region, PvalidateOp::Valid)
    }

    /// Marks a range of pages as invalid for use as private pages.
    fn invalidate_page_range(&self, region: MemoryRegion<PhysAddr>) -> Result<(), SvsmError> {
        pvalidate_page_range(self.env, region, PvalidateOp::Invalid)
    }

    fn configure_alternate_injection(&mut self, alt_inj_requested: bool) -> Result<(), SvsmError> {
        if !alt_inj_requested {
            return Ok(());
        }

        // If alternate injection was requested, then it must be supported by
        // the hypervisor.
        if !hypervisor_ghcb_features().contains(GHCBHvFeatures::SEV_SNP_EXT_INTERRUPTS) {
            return Err(SvsmError::NotSupported);
        }

        APIC_EMULATION_REG_COUNT.store(1, Ordering::Relaxed);
        Ok(())
    }

    fn change_apic_registration_state(&self, incr: bool) -> Result<bool, SvsmError> {
        let mut current = APIC_EMULATION_REG_COUNT.load(Ordering::Relaxed);
        loop {
            let new = if incr {
                // Incrementing is only possible if the registration count
                // has not already dropped to zero, and only if the
                // registration count will not wrap around.
                if current == 0 {
                    return Err(SvsmError::Apic(Registration));
                }
                current
                    .checked_add(1)
                    .ok_or(SvsmError::Apic(Registration))?
            } else {
                // An attempt to decrement when the count is already zero is
                // considered a benign race, which will not result in any
                // actual change but will indicate that emulation is being
                // disabled for the guest.
                if current == 0 {
                    return Ok(false);
                }
                current - 1
            };
            match APIC_EMULATION_REG_COUNT.compare_exchange_weak(
                current,
                new,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Ok(new > 0);
                }
                Err(val) => current = val,
            }
        }
    }

    fn query_apic_registration_state(&self) -> bool {
        APIC_EMULATION_REG_COUNT.load(Ordering::Relaxed) > 0
    }

    fn post_irq(&self, icr: u64) -> Result<(), SvsmError> {
        current_ghcb().hv_ipi(icr)?;
        Ok(())
    }

    fn eoi(&self) {
        // Issue an explicit EOI unless no explicit EOI is required.
        if !current_hv_doorbell().no_eoi_required() {
            // 0x80B is the X2APIC EOI MSR.
            // Errors here cannot be handled but should not be grounds for
            // panic.
            let _ = current_ghcb().wrmsr(0x80B, 0);
        }
    }

    fn start_cpu(&self, cpu: &PerCpu, start_rip: u64) -> Result<(), SvsmError> {
        let pgtable = this_cpu().get_pgtable().clone_shared()?;
        cpu.setup(self, pgtable)?;
        let (vmsa_pa, sev_features) = cpu.alloc_svsm_vmsa(*VTOM as u64, start_rip)?;

        current_ghcb().ap_create(vmsa_pa, cpu.get_apic_id().into(), 0, sev_features)
    }
}
