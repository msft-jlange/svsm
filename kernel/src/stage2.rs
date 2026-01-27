// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2022-2023 SUSE LLC
//
// Author: Joerg Roedel <jroedel@suse.de>

#![no_std]
#![no_main]

pub mod boot_stage2;

use bootdefs::kernel_launch::LOWMEM_END;
use bootdefs::kernel_launch::STAGE2_HEAP_END;
use bootdefs::kernel_launch::STAGE2_HEAP_START;
use bootdefs::kernel_launch::STAGE2_STACK;
use bootdefs::kernel_launch::STAGE2_STACK_END;
use bootdefs::kernel_launch::STAGE2_START;
use bootdefs::kernel_launch::Stage2LaunchInfo;
use bootdefs::platform::SvsmPlatformType;
use core::arch::global_asm;
use core::panic::PanicInfo;
use core::slice;
use cpuarch::snp_cpuid::SnpCpuidTable;
use svsm::address::{Address, PhysAddr, VirtAddr};
use svsm::boot_params::BootParams;
use svsm::console::install_console_logger;
use svsm::cpu::cpuid::{dump_cpuid_table, register_cpuid_table};
use svsm::cpu::flush_tlb_percpu;
use svsm::cpu::gdt::GLOBAL_GDT;
use svsm::cpu::idt::stage2::{early_idt_init, early_idt_init_no_ghcb};
use svsm::cpu::idt::{EARLY_IDT_ENTRIES, IDT, IdtEntry};
use svsm::cpu::percpu::{PERCPU_AREAS, PerCpu, this_cpu};
use svsm::debug::stacktrace::print_stack;
use svsm::error::SvsmError;
use svsm::mm::FixedAddressMappingRange;
use svsm::mm::SVSM_PERCPU_BASE;
use svsm::mm::alloc::memory_info;
use svsm::mm::alloc::print_memory_info;
use svsm::mm::alloc::root_mem_init;
use svsm::mm::init_kernel_mapping_info;
use svsm::mm::pagetable::PTEntry;
use svsm::mm::pagetable::PTEntryFlags;
use svsm::mm::pagetable::PageTable;
use svsm::mm::pagetable::make_private_address;
use svsm::mm::pagetable::paging_init;
use svsm::platform;
use svsm::platform::Stage2PlatformCell;
use svsm::platform::SvsmPlatform;
use svsm::platform::SvsmPlatformCell;
use svsm::platform::init_platform_type;
use svsm::types::PAGE_SIZE;
use svsm::utils::MemoryRegion;

use release::COCONUT_VERSION;

unsafe extern "C" {
    static mut pgtable: PageTable;
    fn switch_to_kernel(entry: u64, initial_stack: u64, platform_type: u64) -> !;
}

#[derive(Debug)]
pub struct KernelPageTablePage<'a> {
    entries: &'a mut [PTEntry],
}

impl KernelPageTablePage<'_> {
    /// # Safety
    /// The caller is required to supply a virtual address that is known to map
    /// a full page of page table or page directory entries.
    unsafe fn new(vaddr: VirtAddr) -> Self {
        // SAFETY: the caller ensures the correctness of the virtual address.
        let entries = unsafe {
            let pte_ptr = vaddr.as_mut_ptr::<PTEntry>();
            slice::from_raw_parts_mut(pte_ptr, svsm::mm::pagetable::ENTRY_COUNT)
        };
        Self { entries }
    }

    fn entry_mut(&mut self, index: usize) -> &mut PTEntry {
        &mut self.entries[index]
    }
}

fn setup_stage2_allocator(heap_start: u64, heap_end: u64) {
    let vstart = VirtAddr::from(heap_start);
    let vend = VirtAddr::from(heap_end);
    let pstart = PhysAddr::from(vstart.bits()); // Identity mapping
    let nr_pages = (vend - vstart) / PAGE_SIZE;

    root_mem_init(pstart, vstart, nr_pages, 0);
}

fn init_percpu(platform: &mut dyn SvsmPlatform) -> Result<(), SvsmError> {
    // SAFETY: this is the first CPU, so there can be no other dependencies
    // on multi-threaded access to the per-cpu areas.
    let percpu_shared = unsafe { PERCPU_AREAS.create_new(0) };
    let bsp_percpu = PerCpu::alloc(percpu_shared)?;
    bsp_percpu.set_current_stack(MemoryRegion::from_addresses(
        VirtAddr::from(STAGE2_STACK_END as u64),
        VirtAddr::from(STAGE2_STACK as u64),
    ));
    // SAFETY: pgtable is properly aligned and is never freed within the
    // lifetime of stage2. We go through a raw pointer to promote it to a
    // static mut. Only the BSP is able to get a reference to it so no
    // aliasing can occur.
    let init_pgtable = unsafe { (&raw mut pgtable).as_mut().unwrap() };
    bsp_percpu.set_pgtable(init_pgtable);
    bsp_percpu.map_self_stage2()?;
    platform.setup_guest_host_comm(bsp_percpu, true);
    Ok(())
}

/// Release all resources in the `PerCpu` instance associated with the current
/// CPU.
///
/// # Safety
///
/// The caller must ensure that the `PerCpu` is never used again.
unsafe fn shutdown_percpu() {
    let ptr = SVSM_PERCPU_BASE.as_mut_ptr::<PerCpu>();
    // SAFETY: ptr is properly aligned but the caller must ensure the PerCpu
    // structure is valid and not aliased.
    unsafe {
        core::ptr::drop_in_place(ptr);
    }
    // SAFETY: pgtable is properly aligned and is never freed within the
    // lifetime of stage2. We go through a raw pointer to promote it to a
    // static mut. Only the BSP is able to get a reference to it so no
    // aliasing can occur.
    let init_pgtable = unsafe { (&raw mut pgtable).as_mut().unwrap() };
    init_pgtable.unmap_4k(SVSM_PERCPU_BASE);
    flush_tlb_percpu();
}

// SAFETY: the caller must guarantee that the IDT specified here will remain
// in scope until a new IDT is loaded.
unsafe fn setup_env(
    boot_params: &BootParams<'_>,
    platform: &mut dyn SvsmPlatform,
    launch_info: &Stage2LaunchInfo,
    cpuid_vaddr: Option<VirtAddr>,
    idt: &mut IDT<'_>,
) {
    GLOBAL_GDT.load_selectors();
    // SAFETY: the caller guarantees that the lifetime of this IDT is suitable.
    unsafe {
        early_idt_init_no_ghcb(idt);
    }

    let debug_serial_port = boot_params.debug_serial_port();
    install_console_logger("Stage2").expect("Console logger already initialized");
    platform
        .env_setup(debug_serial_port, launch_info.vtom.try_into().unwrap())
        .expect("Early environment setup failed");

    let kernel_mapping = FixedAddressMappingRange::new(
        VirtAddr::from(u64::from(STAGE2_START)),
        VirtAddr::from(u64::from(launch_info.stage2_end)),
        PhysAddr::from(u64::from(STAGE2_START)),
    );

    if let Some(cpuid_addr) = cpuid_vaddr {
        // SAFETY: the CPUID page address specified in the launch info was
        // mapped by the loader, which promises to supply a correctly formed
        // CPUID page at that address.
        let cpuid_page = unsafe { &*cpuid_addr.as_ptr::<SnpCpuidTable>() };
        register_cpuid_table(cpuid_page);
    }

    paging_init(platform, true).expect("Failed to initialize early paging");

    // Use the low 640 KB of memory as the heap.
    let lowmem_region =
        MemoryRegion::from_addresses(VirtAddr::from(0u64), VirtAddr::from(u64::from(LOWMEM_END)));
    let heap_mapping = FixedAddressMappingRange::new(
        lowmem_region.start(),
        lowmem_region.end(),
        PhysAddr::from(0u64),
    );
    init_kernel_mapping_info(kernel_mapping, Some(heap_mapping));

    // Now that the heap virtual-to-physical mapping has been established,
    // validate the first 640 KB of memory so it can be used if necessary.
    // SAFETY: the low memory region is known not to overlap any memory in use.
    unsafe {
        platform
            .validate_low_memory(lowmem_region.end().into())
            .expect("failed to validate low 640 KB");
    }

    // Configure the heap.
    setup_stage2_allocator(STAGE2_HEAP_START.into(), STAGE2_HEAP_END.into());

    init_percpu(platform).expect("Failed to initialize per-cpu area");

    // Init IDT again with handlers requiring GHCB (eg. #VC handler)
    // Must be done after init_percpu() to catch early #PFs
    //
    // SAFETY: the caller guarantees that the lifetime of this IDT is suitable.
    unsafe {
        early_idt_init(idt);
    }

    // Complete initializtion of the platform.  After that point, the console
    // will be fully working and any unsupported configuration can be properly
    // reported.
    platform
        .env_setup_late(debug_serial_port)
        .expect("Late environment setup failed");

    if cpuid_vaddr.is_some() {
        dump_cpuid_table();
    }
}

/// Map and validate the specified virtual memory region at the given physical
/// address.  This will fail if the caller specifies a virtual address region
/// that is already mapped.
fn map_page_range(vregion: MemoryRegion<VirtAddr>, paddr: PhysAddr) -> Result<(), SvsmError> {
    let flags = PTEntryFlags::PRESENT
        | PTEntryFlags::WRITABLE
        | PTEntryFlags::ACCESSED
        | PTEntryFlags::DIRTY;

    let mut pgtbl = this_cpu().get_pgtable();
    pgtbl.map_region(vregion, paddr, flags)?;

    Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn stage2_main(launch_info: &Stage2LaunchInfo) -> ! {
    let platform_type = SvsmPlatformType::from(launch_info.platform_type);

    init_platform_type(platform_type);
    let mut platform_cell = SvsmPlatformCell::new(true);
    let platform = platform_cell.platform_mut();
    let stage2_platform_cell = Stage2PlatformCell::new(platform_type);
    let stage2_platform = stage2_platform_cell.platform();

    // SAFETY: the address in the launch info is known to be correct.
    let boot_params = unsafe { BootParams::new(VirtAddr::from(launch_info.boot_params as u64)) }
        .expect("Failed to get boot parameters");

    // Set up space for an early IDT.  This will remain in scope as long as
    // stage2 is in memory.
    let mut early_idt = [IdtEntry::no_handler(); EARLY_IDT_ENTRIES];
    let mut idt = IDT::new(&mut early_idt);

    // Get a reference to the CPUID page if this platform requires it.
    let cpuid_page = stage2_platform.get_cpuid_page(launch_info);

    // SAFETY: the IDT here will remain in scope until the full IDT is
    // initialized later, and thus can safely be used as the early IDT.
    unsafe {
        setup_env(&boot_params, platform, launch_info, cpuid_page, &mut idt);
    }

    // Get the available physical memory region for the kernel
    let kernel_region = boot_params
        .find_kernel_region()
        .expect("Failed to find memory region for SVSM kernel");

    log::info!("SVSM memory region: {kernel_region:#018x}");

    // Set the PML4E of the new kernel page tables in the current page table so
    // the kernel address space is also visible in the current address space.
    // SAFETY: the physical address of the current paging root is known to be
    // identity-mapped in the current address space and therefore that address
    // can be used to obtain a page table view.
    unsafe {
        let vaddr = VirtAddr::new(u64::from(this_cpu().get_pgtable().cr3_value()) as usize);
        let cur_pgtable = slice::from_raw_parts_mut(
            vaddr.as_mut_ptr::<PTEntry>(),
            svsm::mm::pagetable::ENTRY_COUNT,
        );
        let pxe_flags = PTEntryFlags::PRESENT | PTEntryFlags::WRITABLE | PTEntryFlags::ACCESSED;
        cur_pgtable[launch_info.kernel_pml4e_index as usize].set_unrestricted(
            make_private_address(PhysAddr::from(launch_info.kernel_pdpt_paddr)),
            pxe_flags,
        );
    };

    let mem_info = memory_info();
    print_memory_info(&mem_info);

    log::info!(
        "  kernel_region_phys_start = {:#018x}",
        kernel_region.start()
    );
    log::info!("  kernel_region_phys_end   = {:#018x}", kernel_region.end());

    log::info!("Starting SVSM kernel...");

    // SAFETY: the addreses used to invoke the kernel have been calculated
    // correctly for use in the assembly trampoline.
    unsafe {
        // Shut down the PerCpu instance
        shutdown_percpu();

        switch_to_kernel(
            launch_info.kernel_entry,
            launch_info.kernel_stack,
            platform_type as u64,
        );
    };
}

global_asm!(
    r#"
        .globl switch_to_kernel
        switch_to_kernel:

        /* Switch to the kernel stack. */
        movq %rsi, %rsp

        /* Load the platform type into rax as expected by the kernel */
        movq %rdx, %rax

        /* Enter the kernel. */
        push %rdi
        ret
        "#,
    options(att_syntax)
);

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    log::error!("Panic! COCONUT-SVSM Version: {}", COCONUT_VERSION);
    log::error!("Info: {}", info);

    print_stack(3);

    platform::terminate();
}
