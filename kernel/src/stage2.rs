// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2022-2023 SUSE LLC
//
// Author: Joerg Roedel <jroedel@suse.de>

#![no_std]
#![no_main]

pub mod boot_stage2;

use bootdefs::boot_params::BootParamBlock;
use bootdefs::kernel_launch::{
    Stage2LaunchInfo, LOWMEM_END, STAGE2_HEAP_END, STAGE2_HEAP_START, STAGE2_STACK,
    STAGE2_STACK_END, STAGE2_START,
};
use bootdefs::platform::SvsmPlatformType;
use bootimg::prepare_boot_image;
use bootimg::BootImageHost;
use bootimg::BootImageInfo;
use bootimg::BootImageParams;
use core::arch::asm;
use core::mem;
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
use svsm::cpu::idt::{IdtEntry, EARLY_IDT_ENTRIES, IDT};
use svsm::cpu::percpu::{this_cpu, PerCpu, PERCPU_AREAS};
use svsm::debug::stacktrace::print_stack;
use svsm::error::SvsmError;
use svsm::mm::alloc::{memory_info, print_memory_info, root_mem_init};
use svsm::mm::pagetable::{
    make_private_address, paging_init, private_pte_mask, PTEntry, PTEntryFlags, PageTable,
};
use svsm::mm::validate::validate_mapped_region;
use svsm::mm::{
    init_kernel_mapping_info, FixedAddressMappingRange, SVSM_PERCPU_BASE, SVSM_PERTASK_BASE,
};
use svsm::platform;
use svsm::platform::init_platform_type;
use svsm::platform::Stage2Platform;
use svsm::platform::Stage2PlatformCell;
use svsm::platform::SvsmPlatform;
use svsm::platform::SvsmPlatformCell;
use svsm::types::PAGE_SIZE;
use svsm::utils::page_align_up;
use svsm::utils::round_to_pages;
use svsm::utils::MemoryRegion;

use release::COCONUT_VERSION;

extern "C" {
    static mut pgtable: PageTable;
}

struct Stage2BootLoader<'a> {
    map_vaddr: VirtAddr,
    launch_info: &'a Stage2LaunchInfo,
    kernel_region: &'a MemoryRegion<PhysAddr>,
    platform: &'a dyn SvsmPlatform,
    boot_params: &'a BootParams<'a>,
}

impl<'a> Stage2BootLoader<'a> {
    fn new(
        launch_info: &'a Stage2LaunchInfo,
        kernel_region: &'a MemoryRegion<PhysAddr>,
        platform: &'a dyn SvsmPlatform,
        boot_params: &'a BootParams<'_>,
    ) -> Self {
        // The temporary mapping address is initialized as the per-task area.
        // The address range is arbitrary since it is only used for temporary
        // mappings while stage2 is running, and selecting the per-task
        // address space permits temporary mappings without requiring a
        // TLB invalidation after the mappings are used.  The global address
        // range must not be used since that range will be used after
        // preparation of the boot image is complete, and that use must not
        // conflict with temporary mappings that were created during boot
        // image preparation.
        Self {
            launch_info,
            kernel_region,
            map_vaddr: SVSM_PERTASK_BASE,
            platform,
            boot_params,
        }
    }
}

impl<'a> BootImageHost<'a> for Stage2BootLoader<'a> {
    fn display_error(&self, string: &str) {
        log::error!("{}", string);
    }

    fn get_kernel_image(&self) -> Result<&'a [u8], ()> {
        Ok(get_kernel_elf(self.launch_info))
    }

    fn get_kernel_phys_region(&self) -> (u64, u64) {
        let page_count: u64 = round_to_pages(self.kernel_region.len()).try_into().unwrap();
        (self.kernel_region.start().into(), page_count)
    }

    fn get_confidential_address_mask(&self) -> u64 {
        private_pte_mask() as u64
    }

    fn add_page_data(
        &mut self,
        paddr: u64,
        data: Option<&[u8]>,
        total_size: u64,
    ) -> Result<(), ()> {
        let total_size = total_size as usize;
        assert_eq!((total_size & (PAGE_SIZE - 1)), 0);
        // Create a page mapping at the next available virtual address.
        // Since the stage2 page tables will be discarded upon entry to the
        // kernel, the specific mapping address doesn't matter.  The addresses
        // are used in sequence to avoid the need for any TLB flush after
        // the address has been used.
        let map_region = MemoryRegion::new(self.map_vaddr, total_size);
        self.map_vaddr = map_region.end();
        map_page_range(map_region, PhysAddr::new(paddr as usize)).map_err(|_| {
            log::error!("Failed to map kernel memory");
        })?;
        // SAFETY: the virtual address used for mapping is in a portion of the
        // virtual address space unused anywhere else, so it can safely be
        // used for mapping here.
        unsafe {
            validate_mapped_region(self.platform, self.boot_params, map_region).map_err(|_| ())?;

            let target_ptr = map_region.start().as_mut_ptr::<u8>();
            let data_len = match data {
                Some(data_slice) => {
                    assert!(data_slice.len() <= total_size);
                    core::ptr::copy_nonoverlapping(
                        data_slice.as_ptr(),
                        target_ptr,
                        data_slice.len(),
                    );
                    data_slice.len()
                }
                None => 0,
            };

            // Zero the tail end of any partial page.
            if data_len < total_size {
                core::ptr::write_bytes(target_ptr.add(data_len), 0, total_size - data_len);
            }
        }

        Ok(())
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

fn get_kernel_elf<'a>(launch_info: &Stage2LaunchInfo) -> &'static [u8] {
    // Find the bounds of the kernel ELF and load it into the ELF parser
    let elf_start = PhysAddr::from(launch_info.kernel_elf_start as u64);
    let elf_end = PhysAddr::from(launch_info.kernel_elf_end as u64);
    let elf_len = elf_end - elf_start;
    // SAFETY: the base address of the ELF image was selected by the loader and
    // is known not to conflict with any other virtual address mappings.
    unsafe { slice::from_raw_parts(elf_start.bits() as *const u8, elf_len) }
}

fn add_boot_page_data(
    boot_image_host: &mut Stage2BootLoader<'_>,
    paddr: u64,
    data: Option<&[u8]>,
    total_size: u64,
) -> Result<(), ()> {
    boot_image_host
        .add_page_data(paddr, data, total_size)
        .inspect_err(|_| log::error!("Failed to copy unmeasured boot parameters"))
}

fn prepare_kernel_image(
    stage2_platform: &dyn Stage2Platform,
    launch_info: &Stage2LaunchInfo,
    boot_params: &BootParams<'_>,
    boot_image_params: &BootImageParams<'_>,
    boot_image_host: &mut Stage2BootLoader<'_>,
) -> Result<BootImageInfo, ()> {
    let boot_image_info = prepare_boot_image(boot_image_params, boot_image_host)?;

    // Preparing the boot image only mapped those portion of the boot
    // parameters that are measured.  The unmeasured contents need to be loaded
    // now as the loader would load them.  Load them into the kernel area
    // as if they had been prepared as part of the boot image.  The unmeasured
    // boot parameters follow the measured boot parameters, so capture the
    // unmeasured data as the portion of the boot data byte slice that follows
    // the measured parameters.
    let boot_params_measured_size = page_align_up(mem::size_of::<BootParamBlock>());
    let boot_params_unmeasured_size = boot_params.size() - boot_params_measured_size;
    let (_, unmeasured_slice) = boot_params
        .as_byte_slice()
        .split_at(boot_params_measured_size);

    // Copy the data into the kernel image as if it had been prepared as part
    // of the boot image.
    add_boot_page_data(
        boot_image_host,
        boot_image_info.boot_params_paddr + boot_params_measured_size as u64,
        Some(unmeasured_slice),
        boot_params_unmeasured_size as u64,
    )?;

    // Copy the CPUID and secrets pages into the kernel image as if they had
    // been prepared as part of the boot image.
    // SAFETY: the platform guarantees the correctness of the CPUID and secrets
    // page virtual addresses from the stage2 launch info.
    let (cpuid_slice, secrets_slice) = unsafe {
        (
            stage2_platform
                .get_cpuid_page(launch_info)
                .map(|vaddr| slice::from_raw_parts(vaddr.as_ptr::<u8>(), PAGE_SIZE)),
            stage2_platform
                .get_secrets_page(launch_info)
                .map(|vaddr| slice::from_raw_parts(vaddr.as_ptr::<u8>(), PAGE_SIZE)),
        )
    };

    add_boot_page_data(
        boot_image_host,
        boot_image_info.cpuid_paddr,
        cpuid_slice,
        PAGE_SIZE as u64,
    )?;
    add_boot_page_data(
        boot_image_host,
        boot_image_info.secrets_paddr,
        secrets_slice,
        PAGE_SIZE as u64,
    )?;

    Ok(boot_image_info)
}

#[no_mangle]
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

    // Invoke the boot image builder to assemble the kernel image.
    let mut boot_image_host =
        Stage2BootLoader::new(&launch_info, &kernel_region, platform, &boot_params);
    let boot_image_params = BootImageParams {
        boot_params: boot_params.param_block(),
        kernel_fs_start: u64::from(launch_info.kernel_fs_start),
        kernel_fs_end: u64::from(launch_info.kernel_fs_end),
        stage2_start: 0x800000u64,
        vtom: launch_info.vtom,
        debug_serial_port: boot_params.debug_serial_port(),
    };
    let boot_image_info = prepare_kernel_image(
        stage2_platform,
        launch_info,
        &boot_params,
        &boot_image_params,
        &mut boot_image_host,
    )
    .expect("Failed to load kernel image");

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
        cur_pgtable[boot_image_info.kernel_pml4e_index].set_unrestricted(
            make_private_address(PhysAddr::from(boot_image_info.kernel_pdpt_paddr)),
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

        asm!("jmp *%rax",
             in("rax") boot_image_info.context.entry_point,
             in("rdi") boot_image_info.context.launch_info_vaddr,
             in("rsi") platform_type as u64,
             in("rdx") boot_image_info.context.initial_stack,
             in("rcx") boot_image_info.context.initial_stack_base,
             in("r8") boot_image_info.context.paging_root,
             options(att_syntax))
    };

    unreachable!("Road ends here!");
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    log::error!("Panic! COCONUT-SVSM Version: {}", COCONUT_VERSION);
    log::error!("Info: {}", info);

    print_stack(3);

    platform::terminate();
}
