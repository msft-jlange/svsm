// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

use crate::cpu::GuestCpuState;

use cpuarch::vmsa::VMSA;

const APIC_REGISTER_APIC_ID: u64 = 0x802;
const APIC_REGISTER_TPR: u64 = 0x808;
const APIC_REGISTER_PPR: u64 = 0x80A;
const APIC_REGISTER_EOI: u64 = 0x80B;
const APIC_REGISTER_ISR_0: u64 = 0x810;
const APIC_REGISTER_ISR_7: u64 = 0x817;
const APIC_REGISTER_IRR_0: u64 = 0x820;
const APIC_REGISTER_IRR_7: u64 = 0x827;
const APIC_REGISTER_ICR: u64 = 0x830;
const APIC_REGISTER_SELF_IPI: u64 = 0x83F;

#[derive(Clone, Copy, Debug)]
pub enum ApicError {
    ApicError,
}

#[derive(Clone, Copy, Debug)]
pub struct LocalApic {
    apic_id: u32,
    irr: [u32; 8],
    isr_stack_index: usize,
    isr_stack: [u8; 16],
    update_required: bool,
    interrupt_delivered: bool,
    interrupt_queued: bool,
}

impl LocalApic {
    pub fn new(apic_id: u32) -> Self {
        LocalApic {
            apic_id,
            irr: [0; 8],
            isr_stack_index: 0,
            isr_stack: [0; 16],
            update_required: false,
            interrupt_delivered: false,
            interrupt_queued: false,
        }
    }

    pub const fn get_apic_id(&self) -> u32 {
        self.apic_id
    }

    fn scan_irr(&self) -> u8 {
        let mut irq = 0;
        for i in 0..7 {
            let bit_index = self.irr[i].leading_zeros();
            if bit_index < 32 {
                let vector = (i as u32 + 1) * 32 - bit_index;
                irq = vector.try_into().unwrap();
            }
        }
        irq
    }

    fn remove_irr(&mut self, irq: u8) {
        self.irr[irq as usize >> 5] &= !(1 << (irq & 31));
    }

    fn insert_irr(&mut self, irq: u8) {
        self.irr[irq as usize >> 5] |= 1 << (irq & 31);
    }

    fn rewind_pending_interrupt(&mut self, irq: u8) {
        assert!(self.isr_stack_index != 0);
        assert!(self.isr_stack[self.isr_stack_index] == irq);
        self.insert_irr(irq);
        self.isr_stack_index -= 1;
        self.update_required = true;
    }

    pub fn check_delivered_interrupts<T: GuestCpuState>(&mut self, cpu_state: &mut T) {
        // Check to see if a previously delivered interrupt is still pending.
        // If so, move it back to the IRR.
        if self.interrupt_delivered {
            let irq = cpu_state.check_and_clear_pending_interrupt_event();
            if irq != 0 {
                self.rewind_pending_interrupt(irq);
            }
            self.interrupt_delivered = false;
        }

        // Check to see if a previously queued interrupt is still pending.
        // If so, move it back to the IRR.
        if self.interrupt_queued {
            let irq = cpu_state.check_and_clear_pending_virtual_interrupt();
            if irq != 0 {
                self.rewind_pending_interrupt(irq);
            }
            self.interrupt_queued = false;
        }
    }

    fn get_ppr<T: GuestCpuState>(&self, cpu_state: &T) -> u8 {
        // Determine the priority of the current in-service interrupt, if any.
        let ppr = if self.isr_stack_index != 0 {
            self.isr_stack[self.isr_stack_index]
        } else {
            0
        };

        // The PPR is the higher of the in-service interrupt priority and the
        // task priority.
        let tpr: u8 = cpu_state.get_tpr();
        if (ppr >> 4) > (tpr >> 4) {
            ppr
        } else {
            tpr
        }
    }

    fn deliver_interrupt_immediately<T: GuestCpuState>(&mut self, irq: u8, cpu_state: &mut T) -> bool {
        // This interrupt can only be delivered if it is a higher priority
        // than the processor's current priority.
        let ppr = self.get_ppr(cpu_state);
        if ((irq >> 4) <= (ppr >> 4)) || cpu_state.in_interrupt_shadow() {
            false
        } else {
            cpu_state.try_deliver_interrupt_immediately(irq)
        }
    }

    pub fn present_interrupts(&mut self, vmsa: &mut VMSA) {
        if self.update_required {
            // Make sure that all previously delivered interrupts have been
            // processed before attempting to process any more.
            self.check_delivered_interrupts(vmsa);

            let irq = self.scan_irr();
            if irq != 0 {
                // Determine whether this interrupt can be injected
                // immediately.  If not, queue it for delivery when possible.
                if !self.deliver_interrupt_immediately(irq, vmsa) {
                    vmsa.queue_interrupt(irq);
                    self.interrupt_queued = true;
                }

                // Mark this interrupt in-service.  It will be recalled if
                // the ISR is examined again before the interrupt is actually
                // delivered.
                self.remove_irr(irq);
                self.isr_stack_index += 1;
                self.isr_stack[self.isr_stack_index] = irq;
            }
            self.update_required = false;
        }
    }

    pub fn perform_eoi(&mut self) {
        // Pop any in-service interrupt from the stack, and schedule the APIC
        // for reevaluation.
        if self.isr_stack_index != 0 {
            self.isr_stack_index -= 1;
            self.update_required = true;
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

    pub fn read_register<T: GuestCpuState>(
        &mut self,
        cpu_state: &T,
        register: u64,
    ) -> Result<u64, ApicError> {
        match register {
            APIC_REGISTER_APIC_ID => Ok(u64::from(self.apic_id)),
            APIC_REGISTER_IRR_0..=APIC_REGISTER_IRR_7 => {
                let offset = register - APIC_REGISTER_IRR_0;
                let index: usize = offset.try_into().unwrap();
                Ok(self.irr[index] as u64)
            }
            APIC_REGISTER_ISR_0..=APIC_REGISTER_ISR_7 => {
                let offset = register - APIC_REGISTER_IRR_0;
                Ok(self.get_isr(offset.try_into().unwrap()) as u64)
            }
            APIC_REGISTER_TPR => {
                Ok(cpu_state.get_tpr() as u64)
            }
            APIC_REGISTER_PPR => {
                Ok(self.get_ppr(cpu_state) as u64)
            }
            _ => Err(ApicError::ApicError),
        }
    }

    pub fn write_register<T: GuestCpuState>(
        &mut self,
        cpu_state: &mut T,
        register: u64,
        value: u64,
    ) -> Result<(), ApicError> {
        match register {
            APIC_REGISTER_TPR => {
                // TPR must be an 8-bit value.
                if value > 0xFF {
                    Err(ApicError::ApicError)
                } else {
                    cpu_state.set_tpr((value & 0xFF) as u8);
                    Ok(())
                }
            },
            APIC_REGISTER_EOI => {
                self.perform_eoi();
                Ok(())
            },
            APIC_REGISTER_ICR => Err(ApicError::ApicError),
            APIC_REGISTER_SELF_IPI => Err(ApicError::ApicError),
            _ => Err(ApicError::ApicError),
        }
    }
}
