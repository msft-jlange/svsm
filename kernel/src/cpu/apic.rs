// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

use crate::address::VirtAddr;
use crate::cpu::ghcb::current_ghcb;
use crate::cpu::percpu::{this_cpu, PerCpuShared};
use crate::cpu::GuestCpuState;
use crate::error::SvsmError;
use crate::error::SvsmError::Apic;
use crate::mm::GuestPtr;
use crate::requests::SvsmCaa;
use crate::sev::hv_doorbell::{HVDoorbell, HVExtIntInfo, HVExtIntStatus};
use crate::sev::msr_protocol::{hypervisor_ghcb_features, GHCBHvFeatures};
use crate::types::GUEST_VMPL;

use core::sync::atomic::Ordering;

use bitfield_struct::bitfield;

const APIC_REGISTER_APIC_ID: u64 = 0x802;
const APIC_REGISTER_TPR: u64 = 0x808;
const APIC_REGISTER_PPR: u64 = 0x80A;
const APIC_REGISTER_EOI: u64 = 0x80B;
const APIC_REGISTER_ISR_0: u64 = 0x810;
const APIC_REGISTER_ISR_7: u64 = 0x817;
const APIC_REGISTER_TMR_0: u64 = 0x818;
const APIC_REGISTER_TMR_7: u64 = 0x81F;
const APIC_REGISTER_IRR_0: u64 = 0x820;
const APIC_REGISTER_IRR_7: u64 = 0x827;
const APIC_REGISTER_ICR: u64 = 0x830;
const APIC_REGISTER_SELF_IPI: u64 = 0x83F;

#[derive(Debug, PartialEq)]
enum IcrDestFmt {
    Dest = 0,
    OnlySelf = 1,
    AllWithSelf = 2,
    AllButSelf = 3,
}

impl IcrDestFmt {
    const fn into_bits(self) -> u64 {
        self as _
    }
    const fn from_bits(value: u64) -> Self {
        match value {
            3 => Self::AllButSelf,
            2 => Self::AllWithSelf,
            1 => Self::OnlySelf,
            _ => Self::Dest,
        }
    }
}

#[derive(Debug, PartialEq)]
enum IcrMessageType {
    Fixed = 0,
    Unknown = 3,
    Nmi = 4,
    Init = 5,
    Sipi = 6,
    ExtInt = 7,
}

impl IcrMessageType {
    const fn into_bits(self) -> u64 {
        self as _
    }
    const fn from_bits(value: u64) -> Self {
        match value {
            7 => Self::ExtInt,
            6 => Self::Sipi,
            5 => Self::Init,
            4 => Self::Nmi,
            0 => Self::Fixed,
            _ => Self::Unknown,
        }
    }
}

#[bitfield(u64)]
struct ApicIcr {
    pub vector: u8,
    #[bits(3)]
    pub message_type: IcrMessageType,
    pub destination_mode: bool,
    pub delivery_status: bool,
    rsvd_13: bool,
    pub assert: bool,
    pub trigger_mode: bool,
    #[bits(2)]
    pub remote_read_status: usize,
    #[bits(2)]
    pub destination_shorthand: IcrDestFmt,
    #[bits(36)]
    rsvd_55_20: u64,
    #[bits(8)]
    pub destination: usize,
}

#[derive(Clone, Copy, Debug)]
pub enum ApicError {
    ApicError,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LocalApic {
    irr: [u32; 8],
    allowed_irr: [u32; 8],
    isr_stack_index: usize,
    isr_stack: [u8; 16],
    activated: bool,
    update_required: bool,
    interrupt_delivered: bool,
    interrupt_queued: bool,
    lazy_eoi_pending: bool,
    tmr: [u32; 8],
    host_tmr: [u32; 8],
}

impl LocalApic {
    pub fn new() -> Self {
        LocalApic {
            irr: [0; 8],
            allowed_irr: [0; 8],
            isr_stack_index: 0,
            isr_stack: [0; 16],
            activated: false,
            update_required: false,
            interrupt_delivered: false,
            interrupt_queued: false,
            lazy_eoi_pending: false,
            tmr: [0; 8],
            host_tmr: [0; 8],
        }
    }

    pub const fn is_active(&self) -> bool {
        self.activated
    }

    pub fn activate(&mut self) -> Result<(), SvsmError> {
        if self.activated {
            Err(Apic)
        } else {
            self.activated = true;
            Ok(())
        }
    }

    fn scan_irr(&self) -> u8 {
        // Scan to find the highest pending IRR vector.
        for i in (0..7).rev() {
            if self.irr[i] != 0 {
                let bit_index = 31 - self.irr[i].leading_zeros();
                let vector = (i as u32) * 32 + bit_index;
                return vector.try_into().unwrap();
            }
        }
        0
    }

    fn remove_vector_register(register: &mut [u32; 8], irq: u8) {
        register[irq as usize >> 5] &= !(1 << (irq & 31));
    }

    fn insert_vector_register(register: &mut [u32; 8], irq: u8) {
        register[irq as usize >> 5] |= 1 << (irq & 31);
    }

    fn test_vector_register(register: &[u32; 8], irq: u8) -> bool {
        (register[irq as usize >> 5] & 1 << (irq & 31)) != 0
    }

    fn rewind_pending_interrupt(&mut self, irq: u8) {
        assert!(self.isr_stack_index != 0);
        assert!(self.isr_stack[self.isr_stack_index - 1] == irq);
        Self::insert_vector_register(&mut self.irr, irq);
        self.isr_stack_index -= 1;
        self.update_required = true;
    }

    pub fn check_delivered_interrupts<T: GuestCpuState>(
        &mut self,
        cpu_state: &mut T,
        caa_addr: Option<VirtAddr>,
    ) {
        // Check to see if a previously delivered interrupt is still pending.
        // If so, move it back to the IRR.
        if self.interrupt_delivered {
            let irq = cpu_state.check_and_clear_pending_interrupt_event();
            if irq != 0 {
                self.rewind_pending_interrupt(irq);
                self.lazy_eoi_pending = false;
            }
            self.interrupt_delivered = false;
        }

        // Check to see if a previously queued interrupt is still pending.
        // If so, move it back to the IRR.
        if self.interrupt_queued {
            let irq = cpu_state.check_and_clear_pending_virtual_interrupt();
            if irq != 0 {
                self.rewind_pending_interrupt(irq);
                self.lazy_eoi_pending = false;
            }
            self.interrupt_queued = false;
        }

        // If a lazy EOI is pending, then check to see whether an EOI has been
        // requested by the guest.  Note that if a lazy EOI was dismissed
        // above, the guest lazy EOI flag need not be cleared here, since
        // dismissal of any interrupt above will require reprocessing of
        // interrupt state prior to guest reentry, and that reprocessing will
        // reset the guest lazy EOI flag.
        if self.lazy_eoi_pending {
            if let Some(virt_addr) = caa_addr {
                let calling_area = GuestPtr::<SvsmCaa>::new(virt_addr);
                if let Ok(caa) = calling_area.read() {
                    if caa.lazy_eoi_pending == 0 {
                        assert!(self.isr_stack_index != 0);
                        self.perform_eoi();
                    }
                }
            }
        }
    }

    fn get_ppr_with_tpr(&self, tpr: u8) -> u8 {
        // Determine the priority of the current in-service interrupt, if any.
        let ppr = if self.isr_stack_index != 0 {
            self.isr_stack[self.isr_stack_index]
        } else {
            0
        };

        // The PPR is the higher of the in-service interrupt priority and the
        // task priority.
        if (ppr >> 4) > (tpr >> 4) {
            ppr
        } else {
            tpr
        }
    }

    fn get_ppr<T: GuestCpuState>(&self, cpu_state: &T) -> u8 {
        self.get_ppr_with_tpr(cpu_state.get_tpr())
    }

    fn clear_guest_eoi_pending(caa_addr: Option<VirtAddr>) -> Option<GuestPtr<SvsmCaa>> {
        if let Some(virt_addr) = caa_addr {
            let calling_area = GuestPtr::<SvsmCaa>::new(virt_addr);
            // Ignore errors here, since nothing can be done if an error
            // occurs.
            if let Ok(caa) = calling_area.read() {
                let _ = calling_area.write(caa.update_lazy_eoi_pending(0));
            }
            Some(calling_area)
        } else {
            None
        }
    }

    fn deliver_interrupt_immediately<T: GuestCpuState>(
        &mut self,
        irq: u8,
        cpu_state: &mut T,
    ) -> bool {
        if !cpu_state.interrupts_enabled() || cpu_state.in_intr_shadow() {
            false
        } else {
            // This interrupt can only be delivered if it is a higher priority
            // than the processor's current priority.
            let ppr = self.get_ppr(cpu_state);
            if (irq >> 4) <= (ppr >> 4) {
                false
            } else {
                cpu_state.try_deliver_interrupt_immediately(irq)
            }
        }
    }

    pub fn present_interrupts<T: GuestCpuState>(
        &mut self,
        cpu_state: &mut T,
        caa_addr: Option<VirtAddr>,
    ) {
        // Make sure any interrupts being presented by the host have been
        // consumed.
        self.consume_host_interrupts();

        if self.update_required {
            // Make sure that all previously delivered interrupts have been
            // processed before attempting to process any more.
            self.check_delivered_interrupts(cpu_state, caa_addr);

            let irq = self.scan_irr();
            let current_priority = if self.isr_stack_index != 0 {
                self.isr_stack[self.isr_stack_index - 1]
            } else {
                0
            };

            // Assume no lazy EOI can be attempted unless it is recalculated
            // below.
            self.lazy_eoi_pending = false;
            let guest_caa = Self::clear_guest_eoi_pending(caa_addr);

            // This interrupt is a candidate for delivery only if its priority
            // exceeds the priority of the highest priority interrupt currently
            // in service.  This check does not consider TPR, because an
            // interrupt lower in priority than TPR must be queued for delivery
            // as soon as TPR is lowered.
            if (irq & 0xF0) > (current_priority & 0xF0) {
                // Determine whether this interrupt can be injected
                // immediately.  If not, queue it for delivery when possible.
                let try_lazy_eoi = if self.deliver_interrupt_immediately(irq, cpu_state) {
                    // Use of lazy EOI can safely be attempted, because the
                    // highest priority interrupt in service is unambiguous.
                    true
                } else {
                    cpu_state.queue_interrupt(irq);
                    self.interrupt_queued = true;

                    // A lazy EOI can only be attempted if there is no lower
                    // priority interrupt in service.  If a lower priority
                    // interrupt is in service, then the lazy EOI handler
                    // won't know whether the lazy EOI is for the one that
                    // is already in service or the one that is being queued
                    // here.
                    self.isr_stack_index == 0
                };

                // Mark this interrupt in-service.  It will be recalled if
                // the ISR is examined again before the interrupt is actually
                // delivered.
                Self::remove_vector_register(&mut self.irr, irq);
                self.isr_stack[self.isr_stack_index] = irq;
                self.isr_stack_index += 1;

                // Configure a lazy EOI if possible.  Lazy EOI is not possible
                // for level-sensitive interrupts, because an explicit EOI
                // is required to acknowledge the interrupt at the source.
                if try_lazy_eoi && Self::test_vector_register(&self.tmr, irq) {
                    // A lazy EOI is possible only if there is no other
                    // interrupt pending.  If another interrupt is pending,
                    // then an explicit EOI will be required to prompt
                    // delivery of the next interrupt.
                    if self.scan_irr() == 0 {
                        self.lazy_eoi_pending = true;
                        if let Some(calling_area) = guest_caa {
                            if let Ok(caa) = calling_area.read() {
                                if calling_area.write(caa.update_lazy_eoi_pending(1)).is_ok() {
                                    self.lazy_eoi_pending = true;
                                }
                            }
                        }
                    }
                }
            }
            self.update_required = false;
        }
    }

    fn perform_host_eoi(vector: u8) {
        // Errors from the host are not expected and cannot be meaningfully
        // handled, so simply ignore them.
        let _r = current_ghcb().specific_eoi(vector, GUEST_VMPL.try_into().unwrap());
        assert!(_r.is_ok());
    }

    pub fn perform_eoi(&mut self) {
        // Pop any in-service interrupt from the stack, and schedule the APIC
        // for reevaluation.
        if self.isr_stack_index != 0 {
            self.isr_stack_index -= 1;
            let vector = self.isr_stack[self.isr_stack_index];
            if Self::test_vector_register(&self.tmr, vector) {
                if Self::test_vector_register(&self.host_tmr, vector) {
                    Self::perform_host_eoi(vector);
                    Self::remove_vector_register(&mut self.host_tmr, vector);
                } else {
                    // FIXME: should do something with locally generated
                    // level-sensitive interrupts.
                }
                Self::remove_vector_register(&mut self.tmr, vector);
            }
            self.update_required = true;
            self.lazy_eoi_pending = false;
        }
    }

    fn get_isr(&self, index: usize) -> u32 {
        let mut value = 0;
        for i in 0..self.isr_stack_index {
            if (usize::from(self.isr_stack[i] >> 5)) == index {
                value |= 1 << (self.isr_stack[i] & 0x1F)
            }
        }
        value
    }

    fn post_interrupt(&mut self, irq: u8, level_sensitive: bool) {
        // Set the appropriate bit in the IRR.  Once set, signal that interrupt
        // processing is required before returning to the guest.
        Self::insert_vector_register(&mut self.irr, irq);
        if level_sensitive {
            Self::insert_vector_register(&mut self.tmr, irq);
        }
        self.update_required = true;
    }

    pub fn read_register<T: GuestCpuState>(
        &mut self,
        cpu_shared: &PerCpuShared,
        cpu_state: &mut T,
        caa_addr: Option<VirtAddr>,
        register: u64,
    ) -> Result<u64, ApicError> {
        // Rewind any undelivered interrupt so it is reflected in any register
        // read.
        self.check_delivered_interrupts(cpu_state, caa_addr);

        match register {
            APIC_REGISTER_APIC_ID => Ok(u64::from(cpu_shared.apic_id())),
            APIC_REGISTER_IRR_0..=APIC_REGISTER_IRR_7 => {
                let offset = register - APIC_REGISTER_IRR_0;
                let index: usize = offset.try_into().unwrap();
                Ok(self.irr[index] as u64)
            }
            APIC_REGISTER_ISR_0..=APIC_REGISTER_ISR_7 => {
                let offset = register - APIC_REGISTER_IRR_0;
                Ok(self.get_isr(offset.try_into().unwrap()) as u64)
            }
            APIC_REGISTER_TMR_0..=APIC_REGISTER_TMR_7 => {
                let offset = register - APIC_REGISTER_TMR_0;
                let index: usize = offset.try_into().unwrap();
                Ok(self.tmr[index] as u64)
            }
            APIC_REGISTER_TPR => Ok(cpu_state.get_tpr() as u64),
            APIC_REGISTER_PPR => Ok(self.get_ppr(cpu_state) as u64),
            _ => Err(ApicError::ApicError),
        }
    }

    fn handle_icr_write(&mut self, value: u64) -> Result<(), ApicError> {
        let icr = ApicIcr::from(value);

        // Only fixed interrupts can be handled.
        if icr.message_type() != IcrMessageType::Fixed {
            return Err(ApicError::ApicError);
        }

        // Only asserted edge-triggered interrupts can be handled.
        if icr.trigger_mode() || !icr.assert() {
            return Err(ApicError::ApicError);
        }

        // FIXME - support destinations other than self.
        if icr.destination_shorthand() != IcrDestFmt::OnlySelf {
            return Err(ApicError::ApicError);
        }

        self.post_interrupt(icr.vector(), false);

        Ok(())
    }

    pub fn write_register<T: GuestCpuState>(
        &mut self,
        cpu_state: &mut T,
        caa_addr: Option<VirtAddr>,
        register: u64,
        value: u64,
    ) -> Result<(), ApicError> {
        // Rewind any undelivered interrupt so it is correctly processed by
        // any register write.
        self.check_delivered_interrupts(cpu_state, caa_addr);

        match register {
            APIC_REGISTER_TPR => {
                // TPR must be an 8-bit value.
                if value > 0xFF {
                    Err(ApicError::ApicError)
                } else {
                    cpu_state.set_tpr((value & 0xFF) as u8);
                    Ok(())
                }
            }
            APIC_REGISTER_EOI => {
                self.perform_eoi();
                Ok(())
            }
            APIC_REGISTER_ICR => self.handle_icr_write(value),
            APIC_REGISTER_SELF_IPI => {
                if value > 0xFF {
                    Err(ApicError::ApicError)
                } else {
                    self.post_interrupt((value & 0xFF) as u8, false);
                    Ok(())
                }
            }
            _ => Err(ApicError::ApicError),
        }
    }

    pub fn configure_vector(&mut self, vector: u8, allowed: bool) {
        if allowed {
            Self::insert_vector_register(&mut self.allowed_irr, vector);
        } else {
            Self::remove_vector_register(&mut self.allowed_irr, vector);
        }
    }

    fn signal_one_host_interrupt(&mut self, vector: u8, level_sensitive: bool) -> bool {
        // If APIC emulation has not been activated by the guest, then do not
        // filter any host interrupts other than to disallow vectors not in the
        // range 31-255.
        let allowed = if self.activated {
            let index = (vector >> 5) as usize;
            let mask = 1 << (vector & 31);
            (self.allowed_irr[index] & mask) != 0
        } else {
            vector >= 31
        };

        if allowed {
            self.post_interrupt(vector, level_sensitive);
            true
        } else {
            false
        }
    }

    fn signal_several_interrupts(&mut self, group: usize, mut bits: u32) {
        let vector = (group as u8) << 5;
        while bits != 0 {
            let index = 31 - bits.leading_zeros();
            bits &= !(1 << index);
            self.post_interrupt(vector + index as u8, false);
        }
    }

    fn select_interrupt_descriptor(hv_doorbell: &HVDoorbell) -> Option<&HVExtIntInfo> {
        // Select the correct interrupt descriptor based on which
        // interrupt management style is used by the hypervisor.
        if hypervisor_ghcb_features().contains(GHCBHvFeatures::SEV_SNP_MULTI_VMPL) {
            // If there is no event pending for the guest VMPL, then no
            // descriptor is applicable..
            let guest_vmpl_flag = HVExtIntStatus::vmpl_event_mask(GUEST_VMPL);
            if (hv_doorbell.per_vmpl[0].status.load(Ordering::Relaxed) & guest_vmpl_flag) == 0 {
                return None;
            }
            hv_doorbell.per_vmpl[0]
                .status
                .fetch_and(!guest_vmpl_flag, Ordering::Relaxed);

            Some(&hv_doorbell.per_vmpl[GUEST_VMPL])
        } else {
            Some(&hv_doorbell.per_vmpl[0])
        }
    }

    pub fn consume_host_interrupts(&mut self) {
        let hv_doorbell_ref = this_cpu().hv_doorbell();
        if let Some(hv_doorbell) = hv_doorbell_ref {
            // Find the appropriate extended interrupt descriptor for the guest
            // VMPL.
            let descriptor = match Self::select_interrupt_descriptor(hv_doorbell) {
                None => {
                    // Abort the scan if there is no event pending for the
                    // guest VMPL.
                    return;
                }
                Some(descr) => descr,
            };

            // First consume any level-sensitive vector that is present.
            let mut flags = HVExtIntStatus::from(descriptor.status.load(Ordering::Relaxed));
            if flags.level_sensitive() {
                let mut vector;
                // Consume the correct vector atomically.
                loop {
                    let mut new_flags = flags;
                    vector = flags.pending_vector();
                    new_flags.set_pending_vector(0);
                    new_flags.set_level_sensitive(false);
                    if let Err(fail_flags) = descriptor.status.compare_exchange(
                        flags.into(),
                        new_flags.into(),
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        flags = fail_flags.into();
                    } else {
                        flags = new_flags;
                        break;
                    }
                }

                if self.signal_one_host_interrupt(vector, true) {
                    Self::insert_vector_register(&mut self.host_tmr, vector);
                }
            }

            // If a single vector is present, then signal it, otherwise
            // process the entire IRR.
            if flags.multiple_vectors() {
                // Clear the multiple vectors flag first so that additional
                // interrupts are presented via the 8-bit vector.  This must
                // be done before the IRR is scanned so that if additional
                // vectors are presented later, the multiple vectors flag
                // will be set again.
                let multiple_vectors_mask: u32 =
                    HVExtIntStatus::new().with_multiple_vectors(true).into();
                descriptor
                    .status
                    .fetch_and(!multiple_vectors_mask, Ordering::Relaxed);

                // Handle the special case of vector 31.
                if flags.vector_31() {
                    descriptor
                        .status
                        .fetch_and(!(1u32 << 31), Ordering::Relaxed);
                    self.signal_one_host_interrupt(31, false);
                }

                for i in 1..8 {
                    let bits = descriptor.irr[i - 1].swap(0, Ordering::Relaxed);

                    // If APIC emulation has not been activated by the guest,
                    // then do not filter any host interrupts.
                    let allowed_mask = if self.activated {
                        self.allowed_irr[i]
                    } else {
                        !0
                    };

                    self.signal_several_interrupts(i, bits & allowed_mask);
                }
            } else if flags.pending_vector() != 0 {
                // Atomically consume this interrupt.  If it cannot be consumed
                // atomically, then it must be because some other interrupt
                // has been presented, and that can be consumed in another
                // pass.
                let mut new_flags = flags;
                new_flags.set_pending_vector(0);
                if descriptor
                    .status
                    .compare_exchange(
                        flags.into(),
                        new_flags.into(),
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    self.signal_one_host_interrupt(flags.pending_vector(), false);
                }
            }
        }
    }
}
