// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange <jlange@microsoft.com>

use std::error::Error;
use std::mem;
use std::mem::size_of;

use igvm::IgvmDirectiveHeader;
use igvm_defs::{IgvmPageDataFlags, IgvmPageDataType};

use crate::stage2_stack::Stage2Stack;

struct AsmBytes {
    bytes: Vec<u8>,
}

impl AsmBytes {
    fn new() -> Self {
        Self {
            bytes: Vec::default(),
        }
    }

    fn set_offset(&mut self, offset: usize) {
        self.bytes.resize(offset, 0xCC);
    }

    fn write_at_offset(&mut self, offset: usize, bytes: &[u8]) {
        let num_bytes = bytes.len();
        if offset + num_bytes > self.bytes.len() {
            self.bytes.resize(offset + num_bytes, 0xCC);
        }

        for (i, byte) in bytes.iter().enumerate() {
            self.bytes[offset + i] = *byte;
        }
    }

    fn push_bytes_target(&mut self, bytes: &[u8]) -> usize {
        let offset = self.bytes.len();
        self.bytes.extend_from_slice(bytes);
        offset
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn write_u32_at_offset(&mut self, offset: usize, value: u32) {
        self.write_at_offset(
            offset,
            &[
                value as u8,
                (value >> 8) as u8,
                (value >> 16) as u8,
                (value >> 24) as u8,
            ],
        )
    }

    fn push_u32(&mut self, value: u32) {
        self.write_u32_at_offset(self.bytes.len(), value);
    }

    fn take(&mut self) -> Vec<u8> {
        mem::take(&mut self.bytes)
    }

    fn short_jump(&mut self, offset: usize) {
        let current = self.bytes.len();
        let relative: isize = if current < offset {
            (offset - current) as isize
        } else {
            -((current - offset) as isize)
        };
        if relative != (relative as i8) as isize {
            panic!("Short jump offset overflow");
        }

        self.bytes[current - 1] = relative as i8 as u8;
    }

    fn long_jump(&mut self, offset: usize) {
        let current = self.bytes.len();
        let relative: isize = if current < offset {
            (offset - current) as isize
        } else {
            -((current - offset) as isize)
        };
        if relative != (relative as i32) as isize {
            panic!("Long jump offset overflow");
        }

        self.write_u32_at_offset(current - 4, relative as i32 as u32);
    }
}

pub fn create_tdx_reset_page(
    compatibility_mask: u32,
) -> Result<IgvmDirectiveHeader, Box<dyn Error>> {
    let address: u32 = 0xFFFF_F000;
    let mut asm_bytes = AsmBytes::new();

    let initial_rip = 0x10000u32;
    let initial_rsp = initial_rip - size_of::<Stage2Stack>() as u32;

    // Push a constant which holds the vCPU start index.
    asm_bytes.push_u32(0);

    // Add code.
    // cmpl %esi, vCPU_index
    let entry = asm_bytes.push_bytes_target(&[0x3B, 0x35]);
    asm_bytes.push_u32(address);

    // jne entry
    asm_bytes.push_bytes(&[0x75, 0x00]);
    asm_bytes.short_jump(entry);

    // movl start_esp, %esp
    asm_bytes.push_bytes(&[0xBC]);
    asm_bytes.push_u32(initial_rsp);

    // movl stage2_start, %eax
    asm_bytes.push_bytes(&[0xB8]);
    asm_bytes.push_u32(initial_rip);

    // jmp eax
    asm_bytes.push_bytes(&[0xFF, 0xE0]);

    //FF0:
    // jmp entry
    asm_bytes.set_offset(0xFF0);
    asm_bytes.push_bytes(&[0xE9]);
    asm_bytes.push_u32(0);
    asm_bytes.long_jump(entry);

    // Serialize the stream into a page data object.
    Ok(IgvmDirectiveHeader::PageData {
        gpa: address as u64,
        compatibility_mask,
        flags: IgvmPageDataFlags::new(),
        data_type: IgvmPageDataType::NORMAL,
        data: asm_bytes.take(),
    })
}
