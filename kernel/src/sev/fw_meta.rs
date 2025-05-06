// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2022-2023 SUSE LLC
//
// Author: Joerg Roedel <jroedel@suse.de>

use crate::address::PhysAddr;
use crate::error::SvsmError;
use crate::guest_fw::GuestFwInfo;
use crate::types::PAGE_SIZE;
use crate::utils::fw_meta::{find_table, RawMetaBuffer, Uuid};
use zerocopy::{FromBytes, Immutable, KnownLayout};

use core::mem::{size_of, size_of_val};
use core::str::FromStr;

const OVMF_TABLE_FOOTER_GUID: &str = "96b582de-1fb2-45f7-baea-a366c55a082d";
const OVMF_SEV_META_DATA_GUID: &str = "dc886566-984a-4798-a75e-5585a7bf67cc";
const SVSM_INFO_GUID: &str = "a789a612-0597-4c4b-a49f-cbb1fe9d1ddd";

#[derive(Clone, Copy, Debug, FromBytes, KnownLayout, Immutable)]
#[repr(C, packed)]
struct SevMetaDataHeader {
    signature: [u8; 4],
    len: u32,
    version: u32,
    num_desc: u32,
}

#[derive(Clone, Copy, Debug, FromBytes, KnownLayout, Immutable)]
#[repr(C, packed)]
struct SevMetaDataDesc {
    base: u32,
    len: u32,
    t: u32,
}

const SEV_META_DESC_TYPE_MEM: u32 = 1;
const SEV_META_DESC_TYPE_SECRETS: u32 = 2;
const SEV_META_DESC_TYPE_CPUID: u32 = 3;
const SEV_META_DESC_TYPE_CAA: u32 = 4;

/// Parse the firmware metadata from the given slice.
pub fn parse_fw_meta_data(mem: &[u8]) -> Result<GuestFwInfo, SvsmError> {
    let mut meta_data = GuestFwInfo::new();

    let raw_meta = RawMetaBuffer::ref_from_bytes(mem).map_err(|_| SvsmError::Firmware)?;

    // Check the UUID
    let uuid = raw_meta.header.uuid();
    let meta_uuid = Uuid::from_str(OVMF_TABLE_FOOTER_GUID)?;
    if uuid != meta_uuid {
        return Err(SvsmError::Firmware);
    }

    // Get the tables and their length
    let data_len = raw_meta.header.data_len().ok_or(SvsmError::Firmware)?;
    let data_start = size_of_val(&raw_meta.data)
        .checked_sub(data_len)
        .ok_or(SvsmError::Firmware)?;
    let raw_data = raw_meta.data.get(data_start..).ok_or(SvsmError::Firmware)?;

    // First check if this is the SVSM itself instead of OVMF
    let svsm_info_uuid = Uuid::from_str(SVSM_INFO_GUID)?;
    if find_table(&svsm_info_uuid, raw_data).is_some() {
        return Err(SvsmError::Firmware);
    }

    // Search and parse SEV metadata
    parse_sev_meta(&mut meta_data, raw_meta, raw_data)?;

    // Verify that the required elements are present.
    if meta_data.cpuid_page.is_none() {
        log::error!("FW does not specify CPUID_PAGE location");
        return Err(SvsmError::Firmware);
    }

    Ok(meta_data)
}

fn parse_sev_meta(
    meta: &mut GuestFwInfo,
    raw_meta: &RawMetaBuffer,
    raw_data: &[u8],
) -> Result<(), SvsmError> {
    // Find SEV metadata table
    let sev_meta_uuid = Uuid::from_str(OVMF_SEV_META_DATA_GUID)?;
    let Some(tbl) = find_table(&sev_meta_uuid, raw_data) else {
        log::warn!("Could not find SEV metadata in firmware");
        return Ok(());
    };

    // Find the location of the metadata header. We need to adjust the offset
    // since it is computed by taking into account the trailing header and
    // padding, and it is computed backwards.
    let bytes: [u8; 4] = tbl.try_into().map_err(|_| SvsmError::Firmware)?;
    let sev_meta_offset = (u32::from_le_bytes(bytes) as usize)
        .checked_sub(size_of_val(&raw_meta.header) + raw_meta.pad_size())
        .ok_or(SvsmError::Firmware)?;
    // Now compute the start and end of the SEV metadata header
    let sev_meta_start = size_of_val(&raw_meta.data)
        .checked_sub(sev_meta_offset)
        .ok_or(SvsmError::Firmware)?;
    let sev_meta_end = sev_meta_start + size_of::<SevMetaDataHeader>();
    // Bounds check the header and get a pointer to it
    let bytes = raw_meta
        .data
        .get(sev_meta_start..sev_meta_end)
        .ok_or(SvsmError::Firmware)?;
    let sev_meta_hdr = SevMetaDataHeader::ref_from_bytes(bytes).map_err(|_| SvsmError::Firmware)?;

    // Now find the descriptors
    let bytes = &raw_meta.data[sev_meta_end..];
    let num_desc = sev_meta_hdr.num_desc as usize;
    let (descs, _) = <[SevMetaDataDesc]>::ref_from_prefix_with_elems(bytes, num_desc)
        .map_err(|_| SvsmError::Firmware)?;

    for desc in descs {
        let t = desc.t;
        let base = PhysAddr::from(desc.base as usize);
        let len = desc.len as usize;

        match t {
            SEV_META_DESC_TYPE_MEM => meta.add_valid_mem(base, len),
            SEV_META_DESC_TYPE_SECRETS => {
                if len != PAGE_SIZE {
                    return Err(SvsmError::Firmware);
                }
                meta.secrets_page = Some(base);
            }
            SEV_META_DESC_TYPE_CPUID => {
                if len != PAGE_SIZE {
                    return Err(SvsmError::Firmware);
                }
                meta.cpuid_page = Some(base);
            }
            SEV_META_DESC_TYPE_CAA => {
                if len != PAGE_SIZE {
                    return Err(SvsmError::Firmware);
                }
                meta.caa_page = Some(base);
            }
            _ => log::info!("Unknown metadata item type: {}", t),
        }
    }

    Ok(())
}
