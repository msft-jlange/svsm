// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

#[derive(Clone, Copy, Debug)]
pub struct LocalApic {
    apic_id: u32,
}

impl LocalApic {
    pub fn new(apic_id: u32) -> Self {
        LocalApic { apic_id }
    }

    pub const fn get_apic_id(&self) -> u32 {
        self.apic_id
    }
}
