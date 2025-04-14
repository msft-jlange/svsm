// SPDX-License-Identifier: MIT
//
// Copyright (c) SUSE LLC
//
// Author: Joerg Roedel <jroedel@suse.de>

use core::cell::OnceCell;

pub trait ApicAccess: core::fmt::Debug {
    /// Updates the APIC_BASE MSR by reading the current value, applying the
    /// `and_mask`, then the `or_mask`, and writing back the new value.
    ///
    /// # Arguments
    ///
    /// `and_mask` - Value to bitwise AND with the current value.
    /// `or_mask` - Value to bitwise OR with the current value, after
    ///             `and_mask` has been applied.
    fn update_apic_base(&self, and_mask: u64, or_mask: u64);

    /// Write a value to an APIC offset
    ///
    /// # Arguments
    ///
    /// - `offset` - Offset into the APIC
    /// - `value` - Value to write at `offset`
    fn apic_write(&self, offset: usize, value: u64);

    /// Read value from APIC offset
    ///
    /// # Arguments
    ///
    /// - `offset` - Offset into the APIC
    ///
    /// # Returns
    ///
    /// The value read from APIC `offset`.
    fn apic_read(&self, offset: usize) -> u64;
}

/// APIC Base MSR
pub const MSR_APIC_BASE: u32 = 0x1B;

/// End-of-Interrupt register MSR offset
pub const APIC_OFFSET_EOI: usize = 0xB;
/// Spurious-Interrupt-Register MSR offset
pub const APIC_OFFSET_SPIV: usize = 0xF;
/// Interrupt-Service-Register base MSR offset
pub const APIC_OFFSET_ISR: usize = 0x10;
/// Interrupt-Control-Register register MSR offset
pub const APIC_OFFSET_ICR: usize = 0x30;

// SPIV bits
const APIC_SPIV_VECTOR_MASK: u64 = (1u64 << 8) - 1;
const APIC_SPIV_SW_ENABLE_MASK: u64 = 1 << 8;

/// Get the MSR offset relative to a bitmap base MSR and the mask for the MSR
/// value to check for a specific vector bit being set in IRR, ISR, or TMR.
///
/// # Returns
///
/// A `(u32, u32)` tuple with the MSR offset as the first and the vector
/// bitmask as the second value.
fn apic_register_bit(vector: usize) -> (usize, u32) {
    let index: u8 = vector as u8;
    ((index >> 5) as usize, 1 << (index & 0x1F))
}

#[derive(Debug, Default)]
pub struct X86Apic {
    access: OnceCell<&'static dyn ApicAccess>,
}

// APIC enable masks
const APIC_ENABLE_MASK: u64 = 0x800;
const APIC_X2_ENABLE_MASK: u64 = 0x400;

impl X86Apic {
    /// Returns the ApicAccess object.
    fn regs(&self) -> &'static dyn ApicAccess {
        *self.access.get().expect("ApicAccessor not set!")
    }

    /// Initialize the ApicAccessor - Must be called before X86APIC can be used.
    ///
    /// # Arguments
    ///
    /// - `accessor` - Static object implementing [`ApicAccess`] trait.
    ///
    /// # Panics
    ///
    /// This function panics when the `ApicAccessor` has already been set.
    pub fn set_accessor(&self, accessor: &'static dyn ApicAccess) {
        self.access
            .set(accessor)
            .expect("ApicAccessor already set!");
    }

    /// Creates a new instance of [`X86Apic`]
    pub fn new() -> Self {
        Self {
            access: OnceCell::new(),
        }
    }

    /// Enables to APIC in X2APIC mode.
    pub fn enable(&self) {
        let enable_mask: u64 = APIC_ENABLE_MASK | APIC_X2_ENABLE_MASK;
        self.regs().update_apic_base(!enable_mask, enable_mask);
    }

    /// Enable the APIC-Software-Enable bit.
    pub fn sw_enable(&self) {
        self.spiv_write(0xff, true);
    }

    /// Sends an EOI message
    #[inline(always)]
    pub fn eoi(&self) {
        self.regs().apic_write(APIC_OFFSET_EOI, 0);
    }

    /// Writes the APIC ICR register
    ///
    /// # Arguments
    ///
    /// - `icr` - Value to write to the ICR register
    #[inline(always)]
    pub fn icr_write(&self, icr: u64) {
        self.regs().apic_write(APIC_OFFSET_ICR, icr);
    }

    /// Checks whether an IRQ vector is currently in service
    ///
    /// # Arguments
    ///
    /// - `vector` - Vector to check for
    ///
    /// # Returns
    ///
    /// Returns `True` when the vector is in service, `False` otherwise.
    #[inline(always)]
    pub fn check_isr(&self, vector: usize) -> bool {
        // Examine the APIC ISR to determine whether this interrupt vector is
        // active.  If so, it is assumed to be an external interrupt.
        let (offset, mask) = apic_register_bit(vector);
        (self.regs().apic_read(APIC_OFFSET_ISR + offset) & mask as u64) != 0
    }

    /// Set Spurious-Interrupt-Vector Register
    ///
    /// # Arguments
    ///
    /// - `vector` - The IRQ vector to deliver spurious interrupts to.
    /// - `enable` - Value of the APIC-Software-Enable bit.
    #[inline(always)]
    pub fn spiv_write(&self, vector: u8, enable: bool) {
        let apic_spiv: u64 = if enable { APIC_SPIV_SW_ENABLE_MASK } else { 0 }
            | ((vector as u64) & APIC_SPIV_VECTOR_MASK);
        self.regs().apic_write(APIC_OFFSET_SPIV, apic_spiv);
    }
}
