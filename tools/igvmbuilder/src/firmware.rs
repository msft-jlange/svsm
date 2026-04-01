// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2024 SUSE LLC
//
// Author: Roy Hopkins <roy.hopkins@suse.com>

use std::error::Error;

use bootdefs::boot_params::GuestFwInfoBlock;
use bootdefs::boot_params::InitialGuestContext;
use igvm::IgvmDirectiveHeader;

use crate::cmd_options::CmdOptions;
use crate::igvm_builder::Hypervisor;
use crate::igvm_firmware::IgvmFirmware;
use crate::ovmf_firmware::OvmfFirmware;

pub trait Firmware {
    fn directives(&self) -> &Vec<IgvmDirectiveHeader>;
    fn get_guest_context(&self) -> Option<InitialGuestContext>;
    fn get_vtom(&self) -> u64;
    fn get_fw_info(&self) -> GuestFwInfoBlock;
}

pub fn parse_firmware(
    options: &CmdOptions,
    hypervisor: Hypervisor,
    parameter_count: u32,
    compatibility_mask: u32,
) -> Result<Box<dyn Firmware>, Box<dyn Error>> {
    if let Some(filename) = &options.firmware {
        match hypervisor {
            Hypervisor::Qemu => OvmfFirmware::parse(filename, parameter_count, compatibility_mask),
            Hypervisor::HyperV => {
                IgvmFirmware::parse(filename, parameter_count, compatibility_mask)
            }
            Hypervisor::Vanadium => {
                OvmfFirmware::parse(filename, parameter_count, compatibility_mask)
            }
            Hypervisor::Neutral => Err("firmware requires specifying a hypervisor type".into()),
        }
    } else {
        Err("No firmware filename specified".into())
    }
}
