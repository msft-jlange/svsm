// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

extern crate alloc;

use super::pagetable::PTEntryFlags;
use super::vm::{Mapping, VMR};
use super::{SVSM_SHARED_BASE, SVSM_SHARED_END, SVSM_SHARED_STACK_BASE};
use crate::address::VirtAddr;
use crate::error::SvsmError;
use crate::locking::SpinLock;
use crate::types::PAGE_SIZE;

use alloc::sync::Arc;
use bootlib::kernel_launch::KernelLaunchInfo;
use core::cell::OnceCell;

pub static SHARED_VMR: SpinLock<OnceCell<VMR>> = SpinLock::new(OnceCell::new());

pub fn init_global_mem(_launch_info: &KernelLaunchInfo) {
    let guard = SHARED_VMR.lock();

    guard.get_or_init(|| {
        let vmr = VMR::new(SVSM_SHARED_BASE, SVSM_SHARED_END, PTEntryFlags::GLOBAL);
        // SAFETY - this VMR represent the shared address space, which will
        // never be freed.
        unsafe {
            vmr.initialize_from_page_tables();
        }
        vmr
    });
}

pub fn map_shared_stack(mapping: Arc<Mapping>) -> Result<VirtAddr, SvsmError> {
    SHARED_VMR
        .lock()
        .get()
        .unwrap()
        .insert_aligned(SVSM_SHARED_STACK_BASE, mapping, PAGE_SIZE)
}
