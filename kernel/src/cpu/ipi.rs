// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) Microsoft Corporation
//
// Author: Jon Lange (jlange@microsoft.com)

use super::apic::{ApicIcr, IcrDestFmt};
use super::cpuset::{AtomicCpuSet, CpuSet};
use super::idt::common::IPI_VECTOR;
use super::percpu::this_cpu;
use super::percpu::PERCPU_AREAS;
use super::TprGuard;
use crate::error::SvsmError;
use crate::platform::SVSM_PLATFORM;
use crate::types::{TPR_IPI, TPR_SYNCH};
use crate::utils::{ScopedMut, ScopedRef};

use core::cell::Cell;
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug)]
pub enum IpiTarget {
    // A single CPU.
    Single(usize),

    // A set of CPUs.
    Multiple(CpuSet),

    // All CPUs other than the current CPU.
    AllButSelf,

    // All CPUs.
    All,
}

#[derive(Debug)]
pub enum IpiMessage {
    NoIpi,
    NoIpiMut(u32),
}

#[derive(Clone, Copy, Debug)]
pub enum IpiRequest {
    NoIpi,
    IpiMut(*mut IpiMessage),
    IpiShared(*const IpiMessage),
}

impl Default for IpiRequest {
    fn default() -> Self {
        Self::NoIpi
    }
}

#[derive(Debug, Default)]
pub struct IpiBoard {
    // The number of CPUs that have yet to complete the request.
    pending: AtomicUsize,

    // The request description.
    request: IpiRequest,
}

impl IpiBoard {
    pub fn new(request: IpiRequest) -> Self {
        Self {
            request,
            pending: AtomicUsize::new(0),
        }
    }
}

pub fn send_ipi(
    mut target_set: IpiTarget,
    sender_cpu_index: usize,
    ipi_request: IpiRequest,
    cpu_ipi_board: &Cell<*const IpiBoard>,
) {
    // Raise TPR to synch level to prevent reentrant attempts to send an IPI.
    let tpr_guard = TprGuard::raise(TPR_SYNCH);

    // Create a new IPI board to represent this request and install it in the
    // sending CPU structure.
    let ipi_board = IpiBoard::new(ipi_request);

    cpu_ipi_board.set(&ipi_board);

    // Enumerate all CPUs in the target set to advise that an IPI message has
    // been posted.
    let mut include_self = false;
    let mut send_interrupt = false;
    match target_set {
        IpiTarget::Single(cpu_index) => {
            if cpu_index == sender_cpu_index {
                include_self = true;
            } else {
                ipi_board.pending.store(1, Ordering::Relaxed);
                PERCPU_AREAS
                    .get_by_cpu_index(cpu_index)
                    .ipi_from(sender_cpu_index);
                send_interrupt = true;
            }
        }
        IpiTarget::Multiple(ref mut cpu_set) => {
            for cpu_index in cpu_set.iter() {
                if cpu_index == sender_cpu_index {
                    include_self = true;
                } else {
                    ipi_board.pending.fetch_add(1, Ordering::Relaxed);
                    PERCPU_AREAS
                        .get_by_cpu_index(cpu_index)
                        .ipi_from(sender_cpu_index);
                    send_interrupt = true;
                }
            }
            if include_self {
                cpu_set.remove(sender_cpu_index);
            }
        }
        _ => {
            for cpu in PERCPU_AREAS.iter() {
                ipi_board.pending.fetch_add(1, Ordering::Relaxed);
                cpu.as_cpu_ref().ipi_from(sender_cpu_index);
            }
            send_interrupt = true;

            // Remove the current CPU from the target set and completion
            // calculation, since the IPI will be handled locally without
            // requiring the use of an interrupt.
            if let IpiTarget::All = target_set {
                ipi_board.pending.fetch_sub(1, Ordering::Relaxed);
                target_set = IpiTarget::AllButSelf;
                include_self = true;
            }
        }
    }

    // Send the IPI message.
    if send_interrupt {
        send_ipi_irq(target_set).expect("Failed to post IPI interrupt");
    }

    // If sending to the current processor, then handle the message.
    if include_self {
        // Raise TPR to IPI level for consistency with IPI interrupt handling.
        let ipi_tpr_guard = TprGuard::raise(TPR_IPI);

        // SAFETY - the message
        unsafe {
            receive_single_ipi(ipi_request);
        }
        drop(ipi_tpr_guard);
    }

    // Wait until all other CPUs have completed their processing of the
    // message.  This is required to ensure that no other threads can be
    // examining the IPI board.
    //
    // Note that because the current TPR is TPR_SYNCH, which is lower than
    // TPR_IPI, any other IPIs that arrive while waiting here will interrupt
    // this spin loop and will be processed correctly.
    while ipi_board.pending.load(Ordering::Acquire) != 0 {
        core::hint::spin_loop();
    }

    // Clear the bulleting board on the sending CPU.
    cpu_ipi_board.set(ptr::null());

    drop(tpr_guard);
}

fn send_single_ipi_irq(cpu_index: usize, icr: ApicIcr) -> Result<(), SvsmError> {
    let cpu = PERCPU_AREAS.get_by_cpu_index(cpu_index);
    SVSM_PLATFORM.post_irq(icr.with_destination(cpu.apic_id()).into())
}

fn send_ipi_irq(target_set: IpiTarget) -> Result<(), SvsmError> {
    let icr = ApicIcr::new().with_vector(IPI_VECTOR as u8);
    match target_set {
        IpiTarget::Single(cpu_index) => send_single_ipi_irq(cpu_index, icr)?,
        IpiTarget::Multiple(cpu_set) => {
            for cpu_index in cpu_set.iter() {
                send_single_ipi_irq(cpu_index, icr)?;
            }
        }
        IpiTarget::AllButSelf => SVSM_PLATFORM.post_irq(
            icr.with_destination_shorthand(IcrDestFmt::AllButSelf)
                .into(),
        )?,
        IpiTarget::All => SVSM_PLATFORM.post_irq(
            icr.with_destination_shorthand(IcrDestFmt::AllWithSelf)
                .into(),
        )?,
    }
    Ok(())
}

/// # Safety
/// The caller must take responsibility to ensure that the message pointer in
/// the request is valid.  This is normally ensured by assuming the lifetime
/// of the request pointer is protected by the lifetime of the bulletin board
/// that posts it.
unsafe fn receive_single_ipi(request: IpiRequest) {
    match request {
        IpiRequest::NoIpi => {}
        IpiRequest::IpiShared(ptr) => {
            // SAFETY - the validity of this pointer is guaranteed by the
            // caller.
            let msg = unsafe { ScopedRef::new(ptr).unwrap() };
            handle_ipi_message(msg.as_ref());
        }
        IpiRequest::IpiMut(ptr) => {
            // SAFETY - the validity of this pointer is guaranteed by the
            // caller.
            let mut msg = unsafe { ScopedMut::new(ptr).unwrap() };
            handle_ipi_message_mut(msg.as_mut());
        }
    }
}

pub fn handle_ipi_interrupt(request_set: &AtomicCpuSet) {
    // Enumerate all CPUs in the request set and process the request identified
    // by each.
    for cpu_index in request_set.iter(Ordering::Acquire) {
        // Handle the request posted on the bulletin board of the requesting
        // CPU.
        let cpu = PERCPU_AREAS.get_by_cpu_index(cpu_index);

        // SAFETY - The sending CPU's IPI board is known to be valid because
        // the sending CPU is present in the request mask.  The IPI board
        // will be valid at least until this CPU decrements the pending
        // count.
        let ipi_board = unsafe { ScopedRef::new(cpu.ipi_board()).unwrap() };

        // SAFETY - the request message is valid as long as the bulletin board
        // remains valid.
        unsafe {
            receive_single_ipi(ipi_board.request);
        }

        // Now that the request has been handled, decrement the count of
        // pending requests on the sender's bulletin board.  The IPI board
        // may cease to be valid as soon as this decrement completes.
        ipi_board.pending.fetch_sub(1, Ordering::Release);

        drop(ipi_board);
    }
}

fn handle_ipi_message(msg: &IpiMessage) {
    match msg {
        IpiMessage::NoIpi => {}
        IpiMessage::NoIpiMut(_) => {}
    }
}

fn handle_ipi_message_mut(msg: &mut IpiMessage) {
    match msg {
        IpiMessage::NoIpi => {}
        IpiMessage::NoIpiMut(val) => *val = 0,
    }
}

/// Sends an IPI message to multiple CPUs.
///
/// * `target_set` - The set of CPUs to which to send the IPI.
/// * `ipi_message` - The message to send.
pub fn send_multicast_ipi(target_set: IpiTarget, ipi_message: &IpiMessage) {
    this_cpu().send_multicast_ipi(target_set, ipi_message);
}

/// Sends an IPI message to a single CPU.  Because only a single CPU can
/// receive the message, the message object can be mutable.
///
/// * `cpu_index` - The index of the CPU to receive the message.
/// * `ipi_message` - The message to send.
pub fn send_unicast_ipi(cpu_index: usize, ipi_message: &mut IpiMessage) {
    this_cpu().send_unicast_ipi(cpu_index, ipi_message);
}
