// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange <jlange@microsoft.com>

use crate::platform::SvsmPlatform;

#[derive(Clone, Copy, Debug)]
pub struct TdxPlatform {}

impl TdxPlatform {
    pub fn new() -> Self {
        Self {}
    }
}

impl SvsmPlatform for TdxPlatform {
    fn env_setup(&mut self) {}
    fn use_shared_gpa_bit(&self) -> bool {
        true
    }
}
