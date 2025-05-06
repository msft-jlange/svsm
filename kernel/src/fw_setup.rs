// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange <jlange@microsoft.com>

use crate::error::SvsmError;
use crate::guest_fw::GuestFwInfo;
use crate::mm::memory::write_guest_memory_map;

pub fn setup_guest_fw(guest_fw: &GuestFwInfo) -> Result<(), SvsmError> {
    write_guest_memory_map(guest_fw)?;
    copy_tables_to_fw(guest_fw)?;
    prepare_fw_launch(guest_fw)?;
    initialize_guest_vmsa()?;
    register_guest_vmsa()?;

    Ok(())
}
