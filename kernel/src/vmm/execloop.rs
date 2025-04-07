// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

use super::GuestExitMessage;

pub fn enter_guest() -> GuestExitMessage {
    let cpu = this_cpu();
    let mut vmsa_ref = cpu.guest_vmsa_ref();
    let vmsa = vmsa_ref.vmsa();

    loop {
        // Update APIC interrupt emulation state if required.
        cpu.update_apic_emulation(vmsa, caa_addr);

        // Enable the guest VMSA and enter the guest.
        vmsa.enable();

        switch_to_vmpl(GUEST_VMPL as u32);

        // Update mappings again on return from the guest VMPL or halt. If this
        // is an AP it may have been created from the context of another CPU.
        if update_mappings().is_err() {
            // If no mapping exists, then indicate to the caller that the 
            // guest existed with no valid mappings.
            return GuestExitMessage::NoMappings;
        }

    }
}
