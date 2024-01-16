// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

use crate::cpu::percpu::this_cpu_mut;
use crate::protocols::errors::SvsmReqError;
use crate::protocols::RequestParams;

const SVSM_REQ_APIC_QUERY_FEATURES: u32 = 0;
const SVSM_REQ_APIC_READ_REGISTER: u32 = 1;
const SVSM_REQ_APIC_WRITE_REGISTER: u32 = 2;

fn apic_query_features(params: &mut RequestParams) -> Result<(), SvsmReqError> {
    // No features are supported beyond the base feature set.
    params.rcx = 0;
    Ok(())
}

fn apic_read_register(params: &mut RequestParams) -> Result<(), SvsmReqError> {
    let value = this_cpu_mut()
        .read_apic_register(params.rcx)
        .map_err(|_| SvsmReqError::invalid_parameter())?;
    params.rdx = value;
    Ok(())
}

fn apic_write_register(params: &mut RequestParams) -> Result<(), SvsmReqError> {
    this_cpu_mut()
        .write_apic_register(params.rcx, params.rdx)
        .map_err(|_| SvsmReqError::invalid_parameter())
}

pub fn apic_protocol_request(request: u32, params: &mut RequestParams) -> Result<(), SvsmReqError> {
    match request {
        SVSM_REQ_APIC_QUERY_FEATURES => apic_query_features(params),
        SVSM_REQ_APIC_READ_REGISTER => apic_read_register(params),
        SVSM_REQ_APIC_WRITE_REGISTER => apic_write_register(params),
        _ => Err(SvsmReqError::unsupported_call()),
    }
}
