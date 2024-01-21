// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

use crate::cpu::percpu::this_cpu_unsafe;
use crate::sev::ghcb::GHCB;

use core::ops::{Deref, DerefMut};
use core::ptr;

#[derive(Debug)]
pub struct GHCBState {
    ghcb: *mut GHCB,
}

impl GHCBState {
    pub fn new() -> GHCBState {
        GHCBState {
            ghcb: ptr::null_mut(),
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
}

impl Deref for GHCBRef {
    type Target = GHCB;
    fn deref(&self) -> &'static GHCB {
        unsafe {
            let ghcb_state = &*self.ghcb_state;
            &*ghcb_state.ghcb
        }
    }
}

impl DerefMut for GHCBRef {
    fn deref_mut(&mut self) -> &'static mut GHCB {
        unsafe {
            let ghcb_state = &*self.ghcb_state;
            &mut *ghcb_state.ghcb
        }
    }
}

pub fn current_ghcb() -> GHCBRef {
    // FIXME - Add borrow checking to GHCB references.
    unsafe {
        let cpu_ptr = this_cpu_unsafe();
        let cpu = &mut *cpu_ptr;
        let ghcb_state = cpu.ghcb_state();
        GHCBRef { ghcb_state }
    }
}
