// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2022-2023 SUSE LLC
//
// Author: Joerg Roedel <jroedel@suse.de>

use crate::address::{Address, VirtAddr};
use crate::cpu::control_regs::{read_cr4, write_cr4, CR4Flags};
use crate::platform::SVSM_PLATFORM;

use core::arch::asm;

pub fn flush_tlb_global_sync() {
    SVSM_PLATFORM.as_dyn_ref().flush_tlb(None);
}

pub fn flush_tlb_global_percpu() {
    let cr4 = read_cr4();
    write_cr4(cr4 ^ CR4Flags::PGE);
    write_cr4(cr4);
}

pub fn flush_address_percpu(va: VirtAddr) {
    let va: u64 = va.page_align().bits() as u64;
    unsafe {
        asm!("invlpg (%rax)",
             in("rax") va,
             options(att_syntax));
    }
}

pub fn flush_address_sync(va: VirtAddr) {
    SVSM_PLATFORM.as_dyn_ref().flush_tlb(Some(va));
}
