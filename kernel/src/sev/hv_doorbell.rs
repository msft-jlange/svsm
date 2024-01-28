// SPDX-License-Identifier: MIT OR Apache-2.0 Copyright (c) Microsoft Corporation
// Author: Jon Lange (jlange@microsoft.com)

use crate::address::VirtAddr;
use crate::error::SvsmError;
use crate::mm::page_visibility::{make_page_private, make_page_shared};
use crate::mm::virt_to_phys;
use crate::sev::ghcb::GHCB;

use core::sync::atomic::{AtomicU32, Ordering};

use bitfield_struct::bitfield;

#[bitfield(u32)]
pub struct HVExtIntStatus {
    pub pending_vector: u8,
    pub nmi_pending: bool,
    pub mc_pending: bool,
    pub level_sensitive: bool,
    pub ipi_pending: bool,
    pub timer_pending: bool,
    pub guest_msr_access: bool,
    pub multiple_vectors: bool,
    pub no_further_signal: bool,
    pub no_eoi_required: bool,
    pub vmpl1_events: bool,
    pub vmpl2_events: bool,
    pub vmpl3_events: bool,
    #[bits(11)]
    rsvd_30_20: u32,
    pub vector_31: bool,
}

impl HVExtIntStatus {
    pub fn vmpl_event_mask(vmpl: usize) -> u32 {
        let bitmask: u32 = HVExtIntStatus::new().with_vmpl1_events(true).into();
        bitmask << (vmpl - 1)
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct HVExtIntInfo {
    pub status: AtomicU32,
    pub irr: [AtomicU32; 7],
    pub isr: [AtomicU32; 8],
}

#[repr(C)]
#[derive(Debug)]
pub struct HVDoorbell {
    pub per_vmpl: [HVExtIntInfo; 4],
}

impl HVDoorbell {
    pub fn init(vaddr: VirtAddr, ghcb: &mut GHCB) -> Result<(), SvsmError> {
        // The #HV doorbell page must be private before it can be used.
        make_page_shared(vaddr);

        // Register the #HV doorbell page using the GHCB protocol.
        let paddr = virt_to_phys(vaddr);
        ghcb.register_hv_doorbell(paddr).map_err(|e| {
            // Return the page to a private state.
            make_page_private(vaddr);
            e
        })?;

        Ok(())
    }

    /// Processes events specified in the #HV doorbell page, ensuring that
    /// critical events are delivered without being lost.
    pub fn process_events(&self) {
        let flags = self.per_vmpl[0].status.load(Ordering::Relaxed);

        // Clear the no-further-signal bit first.  After this point, additional
        // signals may arrive, but they will block return to lower VMPLs.
        let no_further_signal_mask: u32 = HVExtIntStatus::new().with_no_further_signal(true).into();
        self.per_vmpl[0]
            .status
            .fetch_and(!no_further_signal_mask, Ordering::Relaxed);

        let ipi_pending_mask: u32 = HVExtIntStatus::new().with_ipi_pending(true).into();
        if (flags & ipi_pending_mask) != 0 {
            self.per_vmpl[0]
                .status
                .fetch_and(!ipi_pending_mask, Ordering::Relaxed);
            // IPIs are currently defined to wake only, not to do any work,
            // so no further processing is required.
        }

        let timer_pending_mask: u32 = HVExtIntStatus::new().with_timer_pending(true).into();
        if (flags & timer_pending_mask) != 0 {
            self.per_vmpl[0]
                .status
                .fetch_and(!timer_pending_mask, Ordering::Relaxed);
            // There is no current code to schedule APIC timers, so APIC timer
            // expiration can be ignored.
        }
    }
}
