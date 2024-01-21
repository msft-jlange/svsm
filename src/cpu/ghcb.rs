// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

use crate::cpu::percpu::this_cpu_unsafe;
use crate::sev::ghcb::GHCB;

use core::ops::{Deref, DerefMut};
use core::arch::asm;
use core::ptr;

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub enum GHCBNestingLevel {
    Normal = 0,
    Console = 1,
    Debugger = 2,
}

#[derive(Debug)]
pub struct GHCBState {
    ghcb: *mut GHCB,
    nesting_level: GHCBNestingLevel,
    borrowed: bool,
}

impl GHCBState {
    pub fn new() -> GHCBState {
        GHCBState {
            ghcb: ptr::null_mut(),
            nesting_level: GHCBNestingLevel::Normal,
            borrowed: false,
        }
    }

    pub fn set_ghcb_ptr(&mut self, ghcb: *mut GHCB) {
        self.ghcb = ghcb;
    }

    pub fn ghcb_ptr(&self) -> *mut GHCB {
        return self.ghcb;
    }
}

#[derive(Debug)]
pub struct GHCBRef {
    ghcb_state: *mut GHCBState,
    previous_level: GHCBNestingLevel,
    was_borrowed: bool,
}

impl Deref for GHCBRef {
    type Target = GHCB;
    fn deref(&self) -> &GHCB {
        unsafe {
            let ghcb_state = &*self.ghcb_state;
            &*ghcb_state.ghcb
        }
    }
}

impl DerefMut for GHCBRef {
    fn deref_mut(&mut self) -> &mut GHCB {
        unsafe {
            let ghcb_state = &*self.ghcb_state;
            &mut *ghcb_state.ghcb
        }
    }
}

impl Drop for GHCBRef {
    fn drop(&mut self) {
        unsafe {
            let ghcb_state = &mut *self.ghcb_state;
            assert!(ghcb_state.borrowed);
            ghcb_state.borrowed = self.was_borrowed;
            ghcb_state.nesting_level = self.previous_level;
        }
    }
}

pub fn current_ghcb() -> GHCBRef {
    nested_ghcb(GHCBNestingLevel::Normal)
}

pub fn nested_ghcb(level: GHCBNestingLevel) -> GHCBRef {
    unsafe {
        let cpu_ptr = this_cpu_unsafe();
        let cpu = &mut *cpu_ptr;
        let ghcb_state = cpu.ghcb_state();
        let previous_level = ghcb_state.nesting_level;

        // Recursive borrowing is allowable if the the new nesting level is
        // strictly higher than the old level.
        let was_borrowed = ghcb_state.borrowed;
        if was_borrowed {
            if level <= previous_level {
                panic!("GHCB borrowed recursively");
            }
            asm!("int 3");
        }
        ghcb_state.borrowed = true;
        ghcb_state.nesting_level = level;
        GHCBRef {
            ghcb_state,
            previous_level,
            was_borrowed,
        }
    }
}
