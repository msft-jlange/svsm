// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange <jlange@microsoft.com>

use crate::error::SvsmError;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TdxSuccess {
    Success,
    PageAlreadyAccepted,
    Unknown(u64),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TdxError {
    PageSizeMismatch,
    Unimplemented,
    Unknown(u64),
}

impl From<TdxError> for SvsmError {
    fn from(err: TdxError) -> SvsmError {
        SvsmError::Tdx(err)
    }
}

pub fn tdx_result(err: u64) -> Result<TdxSuccess, TdxError> {
    let code = err >> 32;
    if code < 0x8000_0000 {
        match code {
            0 => Ok(TdxSuccess::Success),
            0x0000_0B0A => Ok(TdxSuccess::PageAlreadyAccepted),
            _ => Ok(TdxSuccess::Unknown(err)),
        }
    } else {
        match code {
            0xC000_0B0B => Err(TdxError::PageSizeMismatch),
            _ => Err(TdxError::Unknown(err)),
        }
    }
}
