// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2024 SUSE LLC
//
// Author: Roy Hopkins <roy.hopkins@suse.com>

use std::error::Error;
use std::mem::size_of;

use igvm::IgvmDirectiveHeader;
use igvm_defs::{IgvmPageDataFlags, IgvmPageDataType, PAGE_SIZE_4K};
use zerocopy::AsBytes;

#[repr(C, packed(1))]
#[derive(AsBytes, Copy, Clone, Default)]
struct SnpCpuidLeaf {
    eax_in: u32,
    ecx_in: u32,
    xcr0: u64,
    xss: u64,
    eax_out: u32,
    ebx_out: u32,
    ecx_out: u32,
    edx_out: u32,
    reserved: u64,
}

impl SnpCpuidLeaf {
    pub fn new1(eax_in: u32) -> Self {
        Self {
            eax_in,
            ecx_in: 0,
            xcr0: 0,
            xss: 0,
            eax_out: 0,
            ebx_out: 0,
            ecx_out: 0,
            edx_out: 0,
            reserved: 0,
        }
    }

    pub fn new2(eax_in: u32, ecx_in: u32) -> Self {
        Self {
            eax_in,
            ecx_in,
            xcr0: 0,
            xss: 0,
            eax_out: 0,
            ebx_out: 0,
            ecx_out: 0,
            edx_out: 0,
            reserved: 0,
        }
    }
}

#[repr(C, packed(1))]
#[derive(AsBytes)]
pub struct SnpCpuidPage {
    count: u32,
    reserved: [u32; 3],
    cpuid_info: [SnpCpuidLeaf; 64],
}

const _: () = assert!(size_of::<SnpCpuidPage>() as u64 <= PAGE_SIZE_4K);

impl Default for SnpCpuidPage {
    fn default() -> Self {
        Self {
            count: 0,
            reserved: [0, 0, 0],
            cpuid_info: [SnpCpuidLeaf::default(); 64],
        }
    }
}

impl SnpCpuidPage {
    pub fn new() -> Result<Self, Box<dyn Error>> {
        let mut cpuid_page = SnpCpuidPage::default();
        cpuid_page.add(SnpCpuidLeaf::new1(0x8000001f))?;
        cpuid_page.add(SnpCpuidLeaf::new2(1, 1))?;
        cpuid_page.add(SnpCpuidLeaf::new1(2))?;
        cpuid_page.add(SnpCpuidLeaf::new1(4))?;
        cpuid_page.add(SnpCpuidLeaf::new2(4, 1))?;
        cpuid_page.add(SnpCpuidLeaf::new2(4, 2))?;
        cpuid_page.add(SnpCpuidLeaf::new2(4, 3))?;
        cpuid_page.add(SnpCpuidLeaf::new1(5))?;
        cpuid_page.add(SnpCpuidLeaf::new1(6))?;
        cpuid_page.add(SnpCpuidLeaf::new1(7))?;
        cpuid_page.add(SnpCpuidLeaf::new2(7, 1))?;
        cpuid_page.add(SnpCpuidLeaf::new1(11))?;
        cpuid_page.add(SnpCpuidLeaf::new2(11, 1))?;
        cpuid_page.add(SnpCpuidLeaf::new1(13))?;
        cpuid_page.add(SnpCpuidLeaf::new2(13, 1))?;
        cpuid_page.add(SnpCpuidLeaf::new1(0x80000001))?;
        cpuid_page.add(SnpCpuidLeaf::new1(0x80000002))?;
        cpuid_page.add(SnpCpuidLeaf::new1(0x80000003))?;
        cpuid_page.add(SnpCpuidLeaf::new1(0x80000004))?;
        cpuid_page.add(SnpCpuidLeaf::new1(0x80000005))?;
        cpuid_page.add(SnpCpuidLeaf::new1(0x80000006))?;
        cpuid_page.add(SnpCpuidLeaf::new1(0x80000007))?;
        cpuid_page.add(SnpCpuidLeaf::new1(0x80000008))?;
        cpuid_page.add(SnpCpuidLeaf::new1(0x8000000a))?;
        cpuid_page.add(SnpCpuidLeaf::new1(0x80000019))?;
        cpuid_page.add(SnpCpuidLeaf::new1(0x8000001a))?;
        cpuid_page.add(SnpCpuidLeaf::new1(0x8000001d))?;
        cpuid_page.add(SnpCpuidLeaf::new2(0x8000001d, 1))?;
        cpuid_page.add(SnpCpuidLeaf::new2(0x8000001d, 2))?;
        cpuid_page.add(SnpCpuidLeaf::new2(0x8000001d, 3))?;
        cpuid_page.add(SnpCpuidLeaf::new1(0x8000001e))?;

        Ok(cpuid_page)
    }

    fn add_directive_as_type(
        &self,
        gpa: u64,
        compatibility_mask: u32,
        data_type: IgvmPageDataType,
        directives: &mut Vec<IgvmDirectiveHeader>,
    ) {
        let mut data = self.as_bytes().to_vec();
        data.resize(PAGE_SIZE_4K as usize, 0);

        directives.push(IgvmDirectiveHeader::PageData {
            gpa,
            compatibility_mask,
            flags: IgvmPageDataFlags::new(),
            data_type,
            data,
        });
    }

    pub fn add_directive(
        &self,
        gpa: u64,
        compatibility_mask: u32,
        directives: &mut Vec<IgvmDirectiveHeader>,
    ) {
        self.add_directive_as_type(
            gpa,
            compatibility_mask,
            IgvmPageDataType::CPUID_DATA,
            directives,
        );
    }

    pub fn add_directive_as_data(
        &self,
        gpa: u64,
        compatibility_mask: u32,
        directives: &mut Vec<IgvmDirectiveHeader>,
    ) {
        self.add_directive_as_type(
            gpa,
            compatibility_mask,
            IgvmPageDataType::NORMAL,
            directives,
        );
    }

    fn add(&mut self, leaf: SnpCpuidLeaf) -> Result<(), Box<dyn Error>> {
        if self.count == 64 {
            return Err("Maximum number of CPUID leaves exceeded".into());
        }
        self.cpuid_info[self.count as usize] = leaf;
        self.count += 1;
        Ok(())
    }
}
