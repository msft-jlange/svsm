// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange <jlange@microsoft.com>

use crate::platform::PlatformEnvironment;

#[derive(Clone, Copy, Debug)]
pub struct Stage2Environment {}

static STAGE2_ENVIRONMENT: Stage2Environment = Stage2Environment::new();

impl Stage2Environment {
    const fn new() -> Self {
        Self {}
    }

    pub fn env() -> &'static Self {
        &STAGE2_ENVIRONMENT
    }
}

impl PlatformEnvironment for Stage2Environment {}
