// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

use crate::address::VirtAddr;
use crate::cpu::idt::common::INT_INJ_VECTOR;
use crate::cpu::percpu::{current_ghcb, this_cpu, PerCpuShared, PERCPU_AREAS};
use crate::error::SvsmError;
use crate::mm::GuestPtr;
use crate::platform::guest_cpu::GuestCpuState;
use crate::platform::SVSM_PLATFORM;
use crate::requests::SvsmCaa;
use crate::sev::hv_doorbell::HVExtIntStatus;
use crate::types::GUEST_VMPL;

use bitfield_struct::bitfield;
use core::sync::atomic::Ordering;

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

pub trait ApicLazyEoi {
    fn no_eoi_required(&self) -> Result<bool, SvsmError>;
    fn set_no_eoi_required(&self, no_eoi_required: bool) -> Result<(), SvsmError>;
}

#[derive(Debug)]
pub struct CaaLazyEoi {
    caa: Option<GuestPtr<SvsmCaa>>,
}

impl CaaLazyEoi {
    pub fn new(caa_addr: Option<VirtAddr>) -> Self {
        Self {
            caa: caa_addr.map(GuestPtr::new),
        }
    }
}

impl ApicLazyEoi for CaaLazyEoi {
    fn no_eoi_required(&self) -> Result<bool, SvsmError> {
        if let Some(ref caa_ptr) = self.caa {
            let caa = caa_ptr.read()?;
            Ok(caa.no_eoi_required != 0)
        } else {
            Ok(false)
        }
    }

    fn set_no_eoi_required(&self, no_eoi_required: bool) -> Result<(), SvsmError> {
        if let Some(ref caa_ptr) = self.caa {
            let mut caa = caa_ptr.read()?;
            if no_eoi_required {
                caa.no_eoi_required = 1;
            } else {
                caa.no_eoi_required = 0;
            }
            caa_ptr.write(caa)
        } else {
            Err(SvsmError::NotSupported)
        }
    }
}

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
    #[bits(12)]
    rsvd_31_20: u64,
    pub destination: u32,
}

#[derive(Clone, Copy, Debug)]
pub enum ApicError {
    ApicError,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct LocalApic {
    irr: [u32; 8],
    allowed_irr: [u32; 8],
    isr_stack_index: usize,
    isr_stack: [u8; 16],
    tmr: [u32; 8],
    host_tmr: [u32; 8],
    update_required: bool,
    interrupt_delivered: bool,
    interrupt_queued: bool,
    lazy_eoi_pending: bool,
}

impl LocalApic {
    pub fn new() -> Self {
        LocalApic {
            irr: [0; 8],
            allowed_irr: [0; 8],
            isr_stack_index: 0,
            isr_stack: [0; 16],
            tmr: [0; 8],
            host_tmr: [0; 8],
            update_required: false,
            interrupt_delivered: false,
            interrupt_queued: false,
            lazy_eoi_pending: false,
        }
    }

    fn scan_irr(&self) -> u8 {
        // Scan to find the highest pending IRR vector.
        for (i, irr) in self.irr.into_iter().enumerate().rev() {
            if irr != 0 {
                let bit_index = 31 - irr.leading_zeros();
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
        let new_index = self.isr_stack_index.checked_sub(1).unwrap();
        assert!(self.isr_stack.get(new_index) == Some(&irq));
        Self::insert_vector_register(&mut self.irr, irq);
        self.isr_stack_index = new_index;
        self.update_required = true;
    }

    pub fn check_delivered_interrupts<T: GuestCpuState, L: ApicLazyEoi>(
        &mut self,
        cpu_state: &mut T,
        lazy_eoi: &L,
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
            if let Ok(no_eoi_required) = lazy_eoi.no_eoi_required() {
                if !no_eoi_required {
                    assert!(self.isr_stack_index != 0);
                    self.perform_eoi();
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

    pub fn consume_pending_ipis(&mut self, cpu_shared: &PerCpuShared) {
        // Scan the IPI IRR vector and transfer any pending IPIs into the local
        // IRR vector.
        for (i, irr) in self.irr.iter_mut().enumerate() {
            *irr |= cpu_shared.ipi_irr_vector(i);
        }
        self.update_required = true;
    }

    pub fn present_interrupts<T: GuestCpuState, L: ApicLazyEoi>(
        &mut self,
        cpu_shared: &PerCpuShared,
        cpu_state: &mut T,
        lazy_eoi: &L,
    ) {
        // Make sure any interrupts being presented by the host have been
        // consumed.
        self.consume_host_interrupts();

        // Consume any pending IPIs.
        if cpu_shared.ipi_pending() {
            self.consume_pending_ipis(cpu_shared);
        }

        if self.update_required {
            // Make sure that all previously delivered interrupts have been
            // processed before attempting to process any more.
            self.check_delivered_interrupts(cpu_state, lazy_eoi);

            let irq = self.scan_irr();
            let current_priority = if self.isr_stack_index != 0 {
                self.isr_stack[self.isr_stack_index - 1]
            } else {
                0
            };

            // Assume no lazy EOI can be attempted unless it is recalculated
            // below.
            self.lazy_eoi_pending = false;
            let _ = lazy_eoi.set_no_eoi_required(false);

            // This interrupt is a candidate for delivery only if its priority
            // exceeds the priority of the highest priority interrupt currently
            // in service.  This check does not consider TPR, because an
            // interrupt lower in priority than TPR must be queued for delivery
            // as soon as TPR is lowered.
            if (irq & 0xF0) > (current_priority & 0xF0) {
                // Determine whether this interrupt can be injected
                // immediately.  If not, queue it for delivery when possible.
                let try_lazy_eoi = if self.deliver_interrupt_immediately(irq, cpu_state) {
                    self.interrupt_delivered = true;

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
                // is required to acknowledge the interrupt at the source.  A
                // lazy EOI is only possible only if there is no other
                // interrupt pending.  If another interrupt is pending, then an
                // explicit EOI will be required to prompt delivery of the
                // next interrupt.
                if try_lazy_eoi
                    && !Self::test_vector_register(&self.tmr, irq)
                    && self.scan_irr() == 0
                    && lazy_eoi.set_no_eoi_required(true).is_ok()
                {
                    // Only track a pending lazy EOI if the
                    // calling area page could successfully be
                    // updated.
                    self.lazy_eoi_pending = true;
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
        for isr in self.isr_stack.into_iter().take(self.isr_stack_index) {
            if (usize::from(isr >> 5)) == index {
                value |= 1 << (isr & 0x1F)
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

    fn send_logical_ipi(&mut self, icr: ApicIcr) -> bool {
        let vector = icr.vector();
        let mut signal = false;

        // Check whether the current CPU matches the destination.
        let destination = icr.destination();
        let apic_id = this_cpu().get_apic_id();
        if Self::logical_destination_match(destination, apic_id) {
            self.post_interrupt(vector, false);
        }

        // Enumerate all CPUs to see which have APIC IDs that match the
        // requested destination.  Skip the current CPU, since it was checked
        // above.
        for cpu_ref in PERCPU_AREAS.iter() {
            let cpu = cpu_ref.unwrap();
            let this_apic_id = cpu.apic_id();
            if (this_apic_id != apic_id)
                && Self::logical_destination_match(destination, this_apic_id)
            {
                cpu.request_ipi(vector);
                signal = true;
            }
        }

        signal
    }

    fn logical_destination_match(destination: u32, apic_id: u32) -> bool {
        // CHeck for a cluster match.
        if (destination >> 16) != (apic_id >> 4) {
            false
        } else {
            let bit = 1u32 << (apic_id & 0xF);
            (destination & bit) != 0
        }
    }

    fn send_physical_ipi(&mut self, icr: ApicIcr) -> bool {
        // If the target APIC ID matches the current processor, then treat this
        // as a self-IPI.  Otherwise, locate the target processor by APIC ID.
        let destination = icr.destination();
        if destination == this_cpu().get_apic_id() {
            self.post_interrupt(icr.vector(), false);
            false
        } else {
            // If the target CPU cannot be located, then simply drop the
            // request.
            if let Some(cpu) = PERCPU_AREAS.get(destination) {
                cpu.request_ipi(icr.vector());
                true
            } else {
                false
            }
        }
    }

    fn send_ipi(&mut self, icr: ApicIcr) {
        let (signal_host, include_others, include_self) = match icr.destination_shorthand() {
            IcrDestFmt::Dest => {
                if icr.destination() == 0xFFFF_FFFF {
                    // This is a broadcast, so treat it as all with self.
                    (true, true, true)
                } else {
                    let signal_host = if icr.destination_mode() {
                        self.send_logical_ipi(icr)
                    } else {
                        self.send_physical_ipi(icr)
                    };

                    // Any possible self-IPI was handled above as part of
                    // delivery to the correct destination.
                    (signal_host, false, false)
                }
            }
            IcrDestFmt::OnlySelf => (false, false, true),
            IcrDestFmt::AllButSelf => (true, true, false),
            IcrDestFmt::AllWithSelf => (true, true, true),
        };

        let vector = icr.vector();

        if include_others {
            // Enumerate all processors in the system except for the
            // current CPU and indicate that an IPI has been requested.
            let apic_id = this_cpu().get_apic_id();
            for cpu_ref in PERCPU_AREAS.iter() {
                let cpu = cpu_ref.unwrap();
                if cpu.apic_id() != apic_id {
                    cpu.request_ipi(vector);
                }
            }
        }

        if include_self {
            self.post_interrupt(vector, false);
        }

        if signal_host {
            // Calculate an ICR value to use for a host IPI request.  This will
            // be a fixed interrupt on the interrupt notification vector using
            // the destination format specified in the ICR value.
            let mut hv_icr = ApicIcr::new()
                .with_vector(INT_INJ_VECTOR as u8)
                .with_message_type(IcrMessageType::Fixed)
                .with_destination_mode(icr.destination_mode())
                .with_destination_shorthand(icr.destination_shorthand())
                .with_destination(icr.destination());

            // Avoid a self interrupt if the target is all-including-self,
            // because the self IPI was delivered above.  In the case of
            // a logical cluster IPI, it is impractical to avoid the self
            // interrupt, but such cases should be rare.
            if hv_icr.destination_shorthand() == IcrDestFmt::AllWithSelf {
                hv_icr.set_destination_shorthand(IcrDestFmt::AllButSelf);
            }

            let _r = SVSM_PLATFORM.as_dyn_ref().post_irq(hv_icr.into());
            assert!(_r.is_ok());
        }
    }

    pub fn read_register<T: GuestCpuState, L: ApicLazyEoi>(
        &mut self,
        cpu_shared: &PerCpuShared,
        cpu_state: &mut T,
        lazy_eoi: &L,
        register: u64,
    ) -> Result<u64, ApicError> {
        // Rewind any undelivered interrupt so it is reflected in any register
        // read.
        self.check_delivered_interrupts(cpu_state, lazy_eoi);

        match register {
            APIC_REGISTER_APIC_ID => Ok(u64::from(cpu_shared.apic_id())),
            APIC_REGISTER_IRR_0..=APIC_REGISTER_IRR_7 => {
                let offset = register - APIC_REGISTER_IRR_0;
                let index: usize = offset.try_into().unwrap();
                Ok(self.irr[index] as u64)
            }
            APIC_REGISTER_ISR_0..=APIC_REGISTER_ISR_7 => {
                let offset = register - APIC_REGISTER_ISR_0;
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

        self.send_ipi(icr);

        Ok(())
    }

    pub fn write_register<T: GuestCpuState, L: ApicLazyEoi>(
        &mut self,
        cpu_state: &mut T,
        lazy_eoi: &L,
        register: u64,
        value: u64,
    ) -> Result<(), ApicError> {
        // Rewind any undelivered interrupt so it is correctly processed by
        // any register write.
        self.check_delivered_interrupts(cpu_state, lazy_eoi);

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
        let index = (vector >> 5) as usize;
        let mask = 1 << (vector & 31);
        if allowed {
            self.allowed_irr[index] |= mask;
        } else {
            self.allowed_irr[index] &= !mask;
        }
    }

    fn signal_one_host_interrupt(&mut self, vector: u8, level_sensitive: bool) -> bool {
        let index = (vector >> 5) as usize;
        let mask = 1 << (vector & 31);
        if (self.allowed_irr[index] & mask) != 0 {
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

    pub fn consume_host_interrupts(&mut self) {
        let hv_doorbell = this_cpu().hv_doorbell().unwrap();
        let vmpl_event_mask = hv_doorbell.per_vmpl_events.swap(0, Ordering::Relaxed);
        // Ignore events other than for the guest VMPL.
        if vmpl_event_mask & (1 << (GUEST_VMPL - 1)) != 0 {
            let descriptor = &hv_doorbell.per_vmpl[GUEST_VMPL - 1];

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
                    self.signal_several_interrupts(i, bits & self.allowed_irr[i]);
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
