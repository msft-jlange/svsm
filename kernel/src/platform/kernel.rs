// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange <jlange@microsoft.com>

use crate::platform::PlatformEnvironment;

#[derive(Clone, Copy, Debug)]
pub struct KernelEnvironment {}

pub static KERNEL_ENVIRONMENT: KernelEnvironment = KernelEnvironment::new();

impl KernelEnvironment {
    const fn new() -> Self {
        Self {}
    }

    pub fn env() -> &'static Self {
        &KERNEL_ENVIRONMENT
    }
}

impl PlatformEnvironment for KernelEnvironment {}
