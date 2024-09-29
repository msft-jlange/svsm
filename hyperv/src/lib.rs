// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

//! This crate provides structures and routines defined by the Hyper-V binary
//! interface.

#![no_std]

pub const HYPERV_SIGNATURE: u32 = 0x31237948; // Hv#1

pub enum CpuidLeaves {
    VendorAndMaxFunction = 0x40000000,
    HvInterface = 0x40000001,
}
