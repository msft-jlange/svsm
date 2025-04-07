// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

use super::GuestExitMessage;

pub fn enter_guest() -> GuestExitMessage {
    let cpu = this_cpu();

    loop {
        // Perform pre-entry vMSA accesses in a separate block so that the vMSA
        // does not remain locked while the guest is running.  This is
        // necessary because another CPU may try to reach into this CPU's VMSA
        // mapping at any time.  Note that this design is full of race
        // conditions, many of which cannot be handled correctly, but there is
        // no better alternative until the SVSM can send its own IPIs after
        // the guest has started.
        {
            let mut vmsa_ref = cpu.guest_vmsa_ref();
            let vmsa = vmsa_ref.vmsa();

            // Update APIC interrupt emulation state if required.
            cpu.update_apic_emulation(vmsa, caa_addr);

            // Enable the guest VMSA and enter the guest.
            vmsa.enable();
        }

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
