// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2022-2023 SUSE LLC
//
// Author: Joerg Roedel <jroedel@suse.de>

pub mod common;
pub mod rwlock;
pub mod spinlock;

pub use common::{IrqGuardLocking, IrqLocking, TprGuardLocking, UnguardedLocking};
pub use rwlock::{
    RWLock, RWLockIrqSafe, RWLockTprSafe, ReadLockGuard, ReadLockGuardIrqSafe,
    ReadLockGuardTprSafe, WriteLockGuard, WriteLockGuardIrqSafe, WriteLockGuardTprSafe,
};
pub use spinlock::{
    RawLockGuard, SpinLock, SpinLockGuard, SpinLockGuardIrqSafe, SpinLockGuardTprSafe,
    SpinLockIrqSafe, SpinLockTprSafe,
};
