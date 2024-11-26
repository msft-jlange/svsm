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

use core::arch::asm;
use core::cell::{Cell, UnsafeCell};
use core::mem::{size_of, MaybeUninit};
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
    NoIpiMut(u32),
}

// Drop is implemented not because any drop is required, but to expressly
// ensure that the type cannot be `Copy`.
impl Drop for IpiMessage {
    fn drop(&mut self) {}
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IpiRequest {
    IpiMut,
    IpiShared,
}

#[derive(Debug)]
pub struct IpiBoard {
    // The number of CPUs that have yet to complete the request.
    pending: AtomicUsize,

    // The request description.
    request: Cell<MaybeUninit<IpiRequest>>,

    // Space to store the IPI message being sent.
    message: UnsafeCell<MaybeUninit<IpiMessage>>,
}

impl IpiBoard {
    pub fn new() -> Self {
        Self {
            request: Cell::new(MaybeUninit::zeroed()),
            pending: AtomicUsize::new(0),
            message: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

pub fn send_ipi(
    mut target_set: IpiTarget,
    sender_cpu_index: usize,
    ipi_message: IpiMessage,
    ipi_request: IpiRequest,
    ipi_board: &IpiBoard,
) -> Option<IpiMessage> {
    // Raise TPR to synch level to prevent reentrant attempts to send an IPI.
    let tpr_guard = TprGuard::raise(TPR_SYNCH);

    // Initialize the IPI board to describe this request.  Since no request
    // can be outstanding right now, the pending count must be zero, and
    // there can be no other CPUs that are have taken references to the IPI
    // board.
    assert_eq!(ipi_board.pending.load(Ordering::Relaxed), 0);
    ipi_board.request.set(MaybeUninit::new(ipi_request));

    // SAFETY; since the IPI board is not yet in use, the message cell can
    // safely be mutated.
    let message = unsafe { &mut *ipi_board.message.get() };

    // Move the IPI message into the IPI board.  It will be dropped or copied
    // out once IPI processing is complete.
    message.write(ipi_message);

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

        // SAFETY: the local IPI board is known to be in the correct state
        // for processing.
        unsafe {
            receive_single_ipi(ipi_board);
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

    // Clear the IPI board.  If this request will return a response, then
    // move the response out of the board; otherwise, drop the message that
    // was placed into the board now that it has been fully consumed.
    let response = if ipi_request == IpiRequest::IpiMut {
        // SAFETY: the message in the IPI board will become uninitialized,
        // so the contents can be moved back into a local variable to be
        // returned.
        unsafe {
            let mut response = MaybeUninit::<IpiMessage>::uninit();
            ptr::copy_nonoverlapping(
                message.as_ptr(),
                response.as_mut_ptr(),
                size_of::<IpiMessage>(),
            );
            Some(response.assume_init())
        }
    } else {
        // SAFETY: The message in the IPI board is initialized and must now
        // be dropped.
        unsafe {
            message.assume_init_drop();
        }
        None
    };

    drop(tpr_guard);

    response
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
unsafe fn receive_single_ipi(board: &IpiBoard) {
    // SAFETY: since the caller has indicated that this IPI board is valid,
    // the request cell is known to be initialized.
    let request = unsafe { board.request.get().assume_init() };
    match request {
        IpiRequest::IpiShared => {
            unsafe { asm!("int 61h") }
            // SAFETY; the IPI message is known to be present and accessible.
            let msg = unsafe {
                let ptr = board.message.get() as *const IpiMessage;
                ScopedRef::<IpiMessage>::new(ptr).unwrap()
            };
            handle_ipi_message(msg.as_ref());
        }
        IpiRequest::IpiMut => {
            unsafe { asm!("int 62h") }
            // SAFETY; the IPI message is known to be present and accessible
            // and not borrowed, making it eligible for a mutable borrow.
            let mut msg = unsafe {
                let ptr = board.message.get() as *mut IpiMessage;
                ScopedMut::<IpiMessage>::new(ptr).unwrap()
            };
            handle_ipi_message_mut(msg.as_mut());
        }
    }
}

pub fn handle_ipi_interrupt(request_set: &AtomicCpuSet) {
    unsafe { asm!("int 50h") }
    // Enumerate all CPUs in the request set and process the request identified
    // by each.
    for cpu_index in request_set.iter(Ordering::Acquire) {
        unsafe { asm!("int 51h") }
        // Handle the request posted on the bulletin board of the requesting
        // CPU.
        let cpu = PERCPU_AREAS.get_by_cpu_index(cpu_index);

        // SAFETY; The IPI board is known to be valid since the sending CPU
        // marked it as valid in this CPU's request bitmap.  The IPI board
        // is guaranteed to remain valid until the pending count is
        // decremented.
        unsafe {
            let ipi_board = cpu.ipi_board();
            receive_single_ipi(cpu.ipi_board());

            // Now that the request has been handled, decrement the count of
            // pending requests on the sender's bulletin board.  The IPI
            // board may cease to be valid as soon as this decrement
            // completes.
            ipi_board.pending.fetch_sub(1, Ordering::Release);
            asm!("int 52h");
        }
    }
}

fn handle_ipi_message(msg: &IpiMessage) {
    unsafe {asm!("int 38h") }
    match msg {
        IpiMessage::NoIpiMut(_) => {}
    }
}

fn handle_ipi_message_mut(msg: &mut IpiMessage) {
    unsafe {asm!("int 30h") }
    match msg {
        IpiMessage::NoIpiMut(val) => *val = 0,
    }
}

/// Sends an IPI message to multiple CPUs.
///
/// # Safety
/// The IPI message must NOT contain any references to data unless that
/// data is known to be in memory that is visible across CPUs/tasks.
/// Otherwise, the recipient could attempt to access a pointer that is
/// invalid in the target context, or - worse - points to completely
/// incorrect data in the target context.
///
/// # Arguments
///
/// * `target_set` - The set of CPUs to which to send the IPI.
/// * `ipi_message` - The message to send.
pub unsafe fn send_multicast_ipi(target_set: IpiTarget, ipi_message: IpiMessage) {
    this_cpu().send_multicast_ipi(target_set, ipi_message);
}

/// Sends an IPI message to a single CPU.  Because only a single CPU can
/// receive the message, the message object can be mutable.
///
/// # Arguments
///
/// * `cpu_index` - The index of the CPU to receive the message.
/// * `ipi_message` - The message to send.
///
/// # Returns
///
/// The response message generated by the IPI recipient.
pub fn send_unicast_ipi(cpu_index: usize, ipi_message: IpiMessage) -> IpiMessage {
    this_cpu().send_unicast_ipi(cpu_index, ipi_message)
}
