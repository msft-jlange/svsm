// SPDX-License-Identifier: (GPL-2.0-or-later OR MIT)
//
// Copyright (c) 2023 SUSE LLC
//
// Author: Nicolai Stange <nstange@suse.de>
//
// vim: ts=4 sw=4 et

#![no_std]

mod addr_range;
mod dynamic;
mod error;
mod file_range;
mod header;
mod load_segments;
mod program_header;
mod relocation;
mod section_header;
mod types;

pub use addr_range::Elf64AddrRange;
pub use dynamic::{Elf64Dynamic, Elf64DynamicRelocTable};
pub use error::ElfError;
pub use file_range::Elf64FileRange;
use header::Elf64Hdr;
pub use load_segments::{
    Elf64ImageLoadSegment, Elf64ImageLoadSegmentIterator, Elf64ImageLoadVaddrAllocInfo,
    Elf64LoadSegments,
};
pub use program_header::{Elf64Phdr, Elf64PhdrFlags};
pub use relocation::{
    Elf64AppliedRelaIterator, Elf64Rela, Elf64Relas, Elf64RelocOp, Elf64RelocProcessor,
    Elf64X86RelocProcessor,
};
pub use section_header::{Elf64Shdr, Elf64ShdrFlags, Elf64ShdrIterator};
pub use types::*;

use core::ffi;

/// This struct represents a parsed 64-bit ELF file. It contains information
/// about the ELF file's header, load segments, dynamic section, and more.
#[derive(Default, Debug, PartialEq)]
pub struct Elf64File<'a> {
    /// Buffer containing the ELF file data
    elf_file_buf: &'a [u8],
    /// The ELF file header
    elf_hdr: Elf64Hdr,
    /// The load segments present in the ELF file
    load_segments: Elf64LoadSegments,
    /// The maximum alignment requirement among load segments
    max_load_segment_align: Elf64Xword,
    /// THe section header string table may not be present
    #[allow(unused)]
    sh_strtab: Option<Elf64Strtab<'a>>,
    dynamic: Option<Elf64Dynamic>,
}

impl<'a> Elf64File<'a> {
    /// This method takes a byte buffer containing the ELF file data and parses
    /// it into an [`Elf64File`] struct, providing access to the ELF file's information.
    ///
    /// # Errors
    ///
    /// Returns an [`ElfError`] if there are issues parsing the ELF file.
    pub fn read(elf_file_buf: &'a [u8]) -> Result<Self, ElfError> {
        let mut elf_hdr = Elf64Hdr::read(elf_file_buf)?;

        // Verify that the program header table is within the file bounds.
        let phdrs_off = usize::try_from(elf_hdr.e_phoff).map_err(|_| ElfError::FileTooShort)?;
        let phdr_size = usize::from(elf_hdr.e_phentsize);
        if phdr_size < 56 {
            return Err(ElfError::InvalidPhdrSize);
        }
        let phdrs_num = usize::from(elf_hdr.e_phnum);
        let phdrs_size = phdrs_num
            .checked_mul(phdr_size)
            .ok_or(ElfError::FileTooShort)?;
        let phdrs_end = phdrs_off
            .checked_add(phdrs_size)
            .ok_or(ElfError::FileTooShort)?;
        if phdrs_end > elf_file_buf.len() {
            return Err(ElfError::FileTooShort);
        }

        // Verify that the section header table is within the file bounds.
        let shdr_size = usize::from(elf_hdr.e_shentsize);
        if shdr_size < 64 {
            return Err(ElfError::InvalidShdrSize);
        }
        if elf_hdr.e_shnum == 0 && elf_hdr.e_shoff != 0 {
            // The number of section headers is stored in the first section header's
            // ->sh_size member.
            elf_hdr.e_shnum = 1;
            Self::check_section_header_table_bounds(&elf_hdr, elf_file_buf.len())?;
            let shdr0 = Self::read_shdr_from_file(elf_file_buf, &elf_hdr, 0);
            elf_hdr.e_shnum = match Elf64Word::try_from(shdr0.sh_size) {
                Ok(shnum) => shnum,
                Err(_) => return Err(ElfError::InvalidSectionIndex),
            };
        }
        Self::check_section_header_table_bounds(&elf_hdr, elf_file_buf.len())?;

        // Verify all headers once at load time, so that no error checking will
        // be needed at each and every subsequent access.
        let mut load_segments = Elf64LoadSegments::new();
        let mut max_load_segment_align = 0;
        let mut dynamic_file_range: Option<Elf64FileRange> = None;
        for i in 0..elf_hdr.e_phnum {
            let phdr = Self::read_phdr_from_file(elf_file_buf, &elf_hdr, i);
            Self::verify_phdr(&phdr, elf_file_buf.len())?;
            if phdr.p_type == Elf64Phdr::PT_LOAD {
                let vaddr_range = phdr.vaddr_range();
                if vaddr_range.vaddr_begin == vaddr_range.vaddr_end {
                    continue;
                }
                if load_segments.try_insert(vaddr_range, i).is_err() {
                    return Err(ElfError::LoadSegmentConflict);
                }
                max_load_segment_align = max_load_segment_align.max(phdr.p_align);
            } else if phdr.p_type == Elf64Phdr::PT_DYNAMIC {
                if dynamic_file_range.is_some() {
                    return Err(ElfError::DynamicPhdrConflict);
                }
                dynamic_file_range = Some(phdr.file_range());
            }
        }

        // If ->e_shstrndx == SHN_XINDEX, the actual strndx is stored in first
        // section header table's ->sh_link member.
        if elf_hdr.e_shstrndx == Elf64Shdr::SHN_XINDEX {
            if elf_hdr.e_shnum == 0 {
                return Err(ElfError::InvalidSectionIndex);
            }
            let shdr0 = Self::read_shdr_from_file(elf_file_buf, &elf_hdr, 0);
            elf_hdr.e_shstrndx = shdr0.sh_link;
        }
        if elf_hdr.e_shstrndx != Elf64Shdr::SHN_UNDEF && elf_hdr.e_shstrndx > elf_hdr.e_shnum {
            return Err(ElfError::InvalidSectionIndex);
        }

        let mut sh_strtab = None;
        for i in 0..elf_hdr.e_shnum {
            let shdr = Self::read_shdr_from_file(elf_file_buf, &elf_hdr, i);
            Self::verify_shdr(&shdr, elf_file_buf.len(), elf_hdr.e_shnum)?;

            if elf_hdr.e_shstrndx != Elf64Shdr::SHN_UNDEF && i == elf_hdr.e_shstrndx {
                if shdr.sh_type != Elf64Shdr::SHT_STRTAB {
                    return Err(ElfError::IncompatibleSectionType);
                }

                let sh_strtab_buf_range = shdr.file_range();
                let sh_strtab_buf =
                    &elf_file_buf[sh_strtab_buf_range.offset_begin..sh_strtab_buf_range.offset_end];
                sh_strtab = Some(Elf64Strtab::new(sh_strtab_buf));
            }
        }

        let dynamic = if let Some(dynamic_file_range) = dynamic_file_range {
            let dynamic_buf =
                &elf_file_buf[dynamic_file_range.offset_begin..dynamic_file_range.offset_end];
            let dynamic = Elf64Dynamic::read(dynamic_buf)?;
            Self::verify_dynamic(&dynamic)?;
            Some(dynamic)
        } else {
            None
        };

        Ok(Self {
            elf_file_buf,
            elf_hdr,
            load_segments,
            max_load_segment_align,
            sh_strtab,
            dynamic,
        })
    }

    /// Reads an ELF Program Header (Phdr) from the ELF file buffer.
    ///
    /// This function reads an ELF Program Header (Phdr) from the provided ELF file buffer
    /// based on the given index `i` and the ELF file header `elf_hdr`.
    ///
    /// # Arguments
    ///
    /// * `elf_file_buf` - The byte buffer containing the ELF file data.
    /// * `elf_hdr` - The ELF file header.
    /// * `i` - The index of the Phdr to read.
    ///
    /// # Returns
    ///
    /// The ELF Program Header (Phdr) at the specified index.
    fn read_phdr_from_file(elf_file_buf: &'a [u8], elf_hdr: &Elf64Hdr, i: Elf64Half) -> Elf64Phdr {
        let phdrs_off = usize::try_from(elf_hdr.e_phoff).unwrap();
        let phdr_size = usize::from(elf_hdr.e_phentsize);
        let i = usize::from(i);
        let phdr_off = phdrs_off + i * phdr_size;
        let phdr_buf = &elf_file_buf[phdr_off..(phdr_off + phdr_size)];
        Elf64Phdr::read(phdr_buf)
    }

    /// Verifies the integrity of an ELF Program Header (Phdr).
    ///
    /// This function verifies the integrity of an ELF Program Header (Phdr). It checks
    /// if the Phdr's type is not PT_NULL and performs additional validation to ensure
    /// the header is valid.
    ///
    /// # Arguments
    ///
    /// * `phdr` - The ELF Program Header (Phdr) to verify.
    /// * `elf_file_buf_len` - The length of the ELF file buffer.
    ///
    /// # Errors
    ///
    /// Returns an [`Err<ElfError>`] if the Phdr is invalid.
    fn verify_phdr(phdr: &Elf64Phdr, elf_file_buf_len: usize) -> Result<(), ElfError> {
        if phdr.p_type == Elf64Phdr::PT_NULL {
            return Ok(());
        }

        phdr.verify()?;

        if phdr.p_filesz != 0 {
            let file_range = phdr.file_range();
            if file_range.offset_end > elf_file_buf_len {
                return Err(ElfError::FileTooShort);
            }
        }

        Ok(())
    }

    /// Reads an ELF Program Header (Phdr) from the ELF file.
    ///
    /// This method reads an ELF Program Header (Phdr) from the ELF file based on the
    /// given index `i`.
    ///
    /// # Arguments
    ///
    /// * `i` - The index of the Phdr to read.
    ///
    /// # Returns
    ///
    /// The ELF Program Header (Phdr) at the specified index.
    fn read_phdr(&self, i: Elf64Half) -> Elf64Phdr {
        Self::read_phdr_from_file(self.elf_file_buf, &self.elf_hdr, i)
    }

    /// Checks if the section header table is within the ELF file bounds.
    ///
    /// This function verifies that the section header table is within the bounds of
    /// the ELF file. It checks the offsets and sizes in the ELF file header.
    ///
    /// # Arguments
    ///
    /// * `elf_hdr` - The ELF file header.
    /// * `elf_file_buf_len` - The length of the ELF file buffer.
    ///
    /// # Errors
    ///
    /// Returns an [`Err<ElfError>`] if the section header table is out of bounds.
    fn check_section_header_table_bounds(
        elf_hdr: &Elf64Hdr,
        elf_file_buf_len: usize,
    ) -> Result<(), ElfError> {
        // Verify that the section header table is within the file bounds.
        let shdrs_off = usize::try_from(elf_hdr.e_shoff).map_err(|_| ElfError::FileTooShort)?;
        let shdr_size = usize::from(elf_hdr.e_shentsize);
        let shdrs_num = usize::try_from(elf_hdr.e_shnum).unwrap();
        let shdrs_size = shdrs_num
            .checked_mul(shdr_size)
            .ok_or(ElfError::FileTooShort)?;
        let shdrs_end = shdrs_off
            .checked_add(shdrs_size)
            .ok_or(ElfError::FileTooShort)?;
        if shdrs_end > elf_file_buf_len {
            return Err(ElfError::FileTooShort);
        }
        Ok(())
    }

    /// Reads an ELF Section Header (Shdr) from the ELF file buffer.
    ///
    /// This function reads an ELF Section Header (Shdr) from the provided ELF file buffer
    /// based on the given index `i` and the ELF file header `elf_hdr`.
    ///
    /// # Arguments
    ///
    /// * `elf_file_buf` - The byte buffer containing the ELF file data.
    /// * `elf_hdr` - The ELF file header.
    /// * `i` - The index of the Shdr to read.
    ///
    /// # Returns
    ///
    /// The ELF Section Header (Shdr) at the specified index.
    fn read_shdr_from_file(elf_file_buf: &'a [u8], elf_hdr: &Elf64Hdr, i: Elf64Word) -> Elf64Shdr {
        let shdrs_off = usize::try_from(elf_hdr.e_shoff).unwrap();
        let shdr_size = usize::from(elf_hdr.e_shentsize);
        let i = usize::try_from(i).unwrap();
        let shdr_off = shdrs_off + i * shdr_size;
        let shdr_buf = &elf_file_buf[shdr_off..(shdr_off + shdr_size)];
        Elf64Shdr::read(shdr_buf)
    }

    /// Verifies the integrity of an ELF Section Header (Shdr).
    ///
    /// This function verifies the integrity of an ELF Section Header (Shdr). It checks
    /// if the Shdr's type is not SHT_NULL and performs additional validation to ensure
    /// the header is valid.
    ///
    /// # Arguments
    ///
    /// * `shdr` - The ELF Section Header (Shdr) to verify.
    /// * `elf_file_buf_len` - The length of the ELF file buffer.
    /// * `shnum` - The number of section headers.
    ///
    /// # Errors
    ///
    /// Returns an [`Err<ElfError>`] if the Shdr is invalid.
    fn verify_shdr(
        shdr: &Elf64Shdr,
        elf_file_buf_len: usize,
        shnum: Elf64Word,
    ) -> Result<(), ElfError> {
        if shdr.sh_type == Elf64Shdr::SHT_NULL {
            return Ok(());
        }

        shdr.verify()?;

        if shdr.sh_link > shnum
            || shdr.sh_flags.contains(Elf64ShdrFlags::INFO_LINK) && shdr.sh_info > shnum
        {
            return Err(ElfError::InvalidSectionIndex);
        }

        if shdr.sh_type != Elf64Shdr::SHT_NOBITS {
            let file_range = shdr.file_range();
            if file_range.offset_end > elf_file_buf_len {
                return Err(ElfError::FileTooShort);
            }
        }

        Ok(())
    }

    /// Reads an ELF Section Header (Shdr) from the ELF file.
    ///
    /// This method reads an ELF Section Header (Shdr) from the ELF file based on the
    /// given index `i`.
    ///
    /// # Arguments
    ///
    /// * `i` - The index of the Shdr to read.
    ///
    /// # Returns
    ///
    /// The ELF Section Header (Shdr) at the specified index.
    fn read_shdr(&self, i: Elf64Word) -> Elf64Shdr {
        Self::read_shdr_from_file(self.elf_file_buf, &self.elf_hdr, i)
    }

    /// Creates an iterator over ELF Section Headers (Shdrs) in the ELF file.
    ///
    /// This method creates an iterator over ELF Section Headers (Shdrs) in the ELF file.
    /// It allows iterating through the section headers for processing.
    ///
    /// # Returns
    ///
    /// An [`Elf64ShdrIterator`] over the ELF Section Headers.
    pub fn shdrs_iter(&self) -> Elf64ShdrIterator<'_> {
        Elf64ShdrIterator::new(self)
    }

    /// Verifies the integrity of the ELF Dynamic section.
    ///
    /// This function verifies the integrity of the ELF Dynamic section.
    ///
    /// # Arguments
    ///
    /// * `dynamic` - The ELF Dynamic section to verify.
    ///
    /// # Errors
    ///
    /// Returns an [`Err<ElfError>`] if the Dynamic section is invalid.
    fn verify_dynamic(dynamic: &Elf64Dynamic) -> Result<(), ElfError> {
        dynamic.verify()?;
        Ok(())
    }

    /// Maps a virtual address (Vaddr) range to a corresponding file offset.
    ///
    /// This function maps a given virtual address (Vaddr) range to the corresponding
    /// file offset within the ELF file. It takes the beginning `vaddr_begin` and an
    /// optional `vaddr_end` (if provided), and returns the corresponding file offset
    /// range as an [`Elf64FileRange`].
    ///
    /// # Arguments
    ///
    /// * `vaddr_begin` - The starting virtual address of the range.
    /// * `vaddr_end` - An optional ending virtual address of the range. If not provided,
    ///   the function assumes a range of size 1.
    ///
    /// # Returns
    ///
    /// A [`Result`] containing an [`Elf64FileRange`] representing the file offset range.
    ///
    /// # Errors
    ///
    /// Returns an [`Err<ElfError>`] in the following cases:
    ///
    /// * If `vaddr_begin` is [`Elf64Addr::MAX`], indicating an unmapped virtual address range.
    /// * If the virtual address range is not found within any loaded segment.
    /// * If the file offset calculations result in an invalid file range.
    /// * If the virtual address range extends beyond the loaded segment's file content,
    ///   indicating an unbacked virtual address range.
    fn map_vaddr_to_file_off(
        &self,
        vaddr_begin: Elf64Addr,
        vaddr_end: Option<Elf64Addr>,
    ) -> Result<Elf64FileRange, ElfError> {
        if vaddr_begin == Elf64Addr::MAX {
            return Err(ElfError::UnmappedVaddrRange);
        }
        let vaddr_range = Elf64AddrRange {
            vaddr_begin,
            vaddr_end: vaddr_end.unwrap_or(vaddr_begin + 1),
        };
        let (phdr_index, offset) = match self.load_segments.lookup_vaddr_range(&vaddr_range) {
            Some(load_segment) => (load_segment.0, load_segment.1),
            None => return Err(ElfError::UnmappedVaddrRange),
        };

        let phdr = self.read_phdr(phdr_index);
        let segment_file_range = phdr.file_range();
        let offset_in_segment = usize::try_from(offset).map_err(|_| ElfError::InvalidFileRange)?;
        let offset_begin = segment_file_range
            .offset_begin
            .checked_add(offset_in_segment)
            .ok_or(ElfError::InvalidFileRange)?;
        let offset_end = match vaddr_end {
            Some(vaddr_end) => {
                let len = vaddr_end - vaddr_begin;
                let len = usize::try_from(len).map_err(|_| ElfError::InvalidFileRange)?;
                let offset_end = offset_begin
                    .checked_add(len)
                    .ok_or(ElfError::InvalidFileRange)?;

                // A PT_LOAD segment is not necessarily backed completely by ELF
                // file content: ->p_filesz can be <= ->memsz.
                if offset_end > segment_file_range.offset_end {
                    return Err(ElfError::UnbackedVaddrRange);
                }

                offset_end
            }
            None => {
                // The query did not specify an end address, as can e.g. happen
                // when examining some table referenced from .dynamic with
                // unknown size.  Return the upper segment bound.
                segment_file_range.offset_end
            }
        };
        Ok(Elf64FileRange {
            offset_begin,
            offset_end,
        })
    }

    /// Maps a virtual address range to a slice of bytes from the ELF file buffer.
    ///
    /// This function takes a virtual address range specified by `vaddr_begin` and
    /// optionally `vaddr_end` and maps it to a slice of bytes from the ELF file buffer.
    ///
    /// If `vaddr_end` is [`Some`], the function maps the range from `vaddr_begin` (inclusive)
    /// to `vaddr_end` (exclusive) to a slice of bytes from the ELF file buffer.
    ///
    /// If `vaddr_end` is [`None`], the function maps the range from `vaddr_begin` to the end
    /// of the virtual address range associated with `vaddr_begin` to a slice of bytes from
    /// the ELF file buffer.
    ///
    /// # Arguments
    ///
    /// * `vaddr_begin` - The starting virtual address of the range to map.
    /// * `vaddr_end` - An optional ending virtual address of the range to map.
    ///
    /// # Returns
    ///
    /// - [`Ok<slice>`]: If the virtual address range is valid and successfully mapped, returns
    ///   a reference to the corresponding slice of bytes from the ELF file buffer.
    /// - [`Err<ElfError>`]: If an error occurs during mapping, such as an unmapped or unbacked
    ///   virtual address range, returns an [`ElfError`].
    fn map_vaddr_to_file_buf(
        &self,
        vaddr_begin: Elf64Addr,
        vaddr_end: Option<Elf64Addr>,
    ) -> Result<&[u8], ElfError> {
        let file_range = self.map_vaddr_to_file_off(vaddr_begin, vaddr_end)?;
        Ok(&self.elf_file_buf[file_range.offset_begin..file_range.offset_end])
    }

    // For PIE executables, relieve the using code from offset calculations due
    // to address alignment. Do it here and consistently. The address passed here
    // may be either
    // - a load address corresponding as-is to the first load segment's beginning or
    // - the load address corresponding as-is to the first load segment's
    //   beginning rounded down to match the alginment constraints.
    // The passed address will be mapped to the first variant in either case.
    fn image_load_addr(&self, image_load_addr: Elf64Addr) -> Elf64Addr {
        if self.max_load_segment_align == 0 {
            image_load_addr
        } else {
            let aligned_image_load_addr = image_load_addr & !(self.max_load_segment_align - 1);
            aligned_image_load_addr + self.image_load_align_offset()
        }
    }

    /// Calculates the alignment offset for the image load address.
    ///
    /// This function calculates the alignment offset that needs to be applied to the
    /// image's load address to ensure alignment with the maximum alignment constraint
    /// of all load segments.
    ///
    /// If the `max_load_segment_align` is 0 (indicating no alignment constraints), the
    /// offset is 0.
    ///
    /// If there are load segments with alignment constraints, this function determines
    /// the offset needed to align the image load address with the maximum alignment
    /// constraint among all load segments.
    ///
    /// # Returns
    ///
    /// - [`Elf64Off`]: The alignment offset for the image's load address.
    fn image_load_align_offset(&self) -> Elf64Off {
        if self.max_load_segment_align == 0 {
            return 0;
        }

        // The first segment loaded is not necessarily aligned to the maximum of
        // all segment alignment constraints. Determine the offset from the next
        // lower aligned address to the first segment's beginning.
        self.load_segments.total_vaddr_range().vaddr_begin & (self.max_load_segment_align - 1)
    }

    // The ELF "base address" has a well-defined meaning: it is defined in the
    // spec as the difference between the lowest address of the actual memory
    // image the file has been loaded into and the lowest vaddr of all the
    // PT_LOAD program headers. Calculate it in two's complement representation.
    fn load_base(&self, image_load_addr: Elf64Addr) -> Elf64Xword {
        let image_load_addr = self.image_load_addr(image_load_addr);
        image_load_addr.wrapping_sub(self.load_segments.total_vaddr_range().vaddr_begin)
    }

    pub fn image_load_vaddr_alloc_info(&self) -> Elf64ImageLoadVaddrAllocInfo {
        let mut range = self.load_segments.total_vaddr_range();

        if self.max_load_segment_align != 0 {
            range.vaddr_begin &= !(self.max_load_segment_align - 1);
        }

        let pie = self.dynamic.as_ref().map(|d| d.is_pie()).unwrap_or(false);
        let align = if pie {
            Some(self.max_load_segment_align)
        } else {
            None
        };

        Elf64ImageLoadVaddrAllocInfo { range, align }
    }

    /// Creates an iterator over the ELF segments that are part of the loaded image.
    ///
    /// This function returns an iterator that allows iterating over the ELF segments
    /// belonging to the loaded image. It takes the `image_load_addr`, which represents
    /// the virtual address where the ELF image is loaded in memory. The iterator yields
    /// [`load_segments::Elf64ImageLoadSegment`] instances.
    ///
    /// # Arguments
    ///
    /// * `image_load_addr` - The virtual address where the ELF image is loaded in memory.
    ///
    /// # Returns
    ///
    /// An [`Elf64ImageLoadSegmentIterator`] over the loaded image segments.
    pub fn image_load_segment_iter(
        &'a self,
        image_load_addr: Elf64Addr,
    ) -> Elf64ImageLoadSegmentIterator<'a> {
        let load_base = self.load_base(image_load_addr);
        Elf64ImageLoadSegmentIterator {
            elf_file: self,
            load_base,
            next: 0,
        }
    }

    ///
    /// This function processes dynamic relocations (relas) in the ELF file and applies them
    /// to the loaded image. It takes a generic `rela_proc` parameter that should implement the
    /// [`Elf64RelocProcessor`] trait, allowing custom relocation processing logic.
    ///
    /// The `image_load_addr` parameter specifies the virtual address where the ELF image is
    /// loaded in memory.
    ///
    /// # Arguments
    ///
    /// * `rela_proc` - A relocation processor implementing the [`Elf64RelocProcessor`] trait.
    /// * `image_load_addr` - The virtual address where the ELF image is loaded in memory.
    ///
    /// # Returns
    ///
    /// - [`Ok<Some<iterator>>`]: If relocations are successfully applied, returns an iterator
    ///   over the applied relocations.
    /// - [`Ok<None>`]: If no relocations are present or an error occurs during processing,
    ///   returns [`None`].
    /// - [`Err<ElfError>`]: If an error occurs while processing relocations, returns an
    ///   [`ElfError`].
    pub fn apply_dyn_relas<RP: Elf64RelocProcessor>(
        &'a self,
        rela_proc: RP,
        image_load_addr: Elf64Addr,
    ) -> Result<Option<Elf64AppliedRelaIterator<'a, RP>>, ElfError> {
        let dynamic = match &self.dynamic {
            Some(dynamic) => dynamic,
            None => return Ok(None),
        };
        let dynamic_rela = match &dynamic.rela {
            Some(dynamic_rela) => dynamic_rela,
            None => return Ok(None),
        };

        let load_base = self.load_base(image_load_addr);

        let relas_file_range = dynamic_rela.vaddr_range();
        let relas_buf = self.map_vaddr_to_file_buf(
            relas_file_range.vaddr_begin,
            Some(relas_file_range.vaddr_end),
        )?;
        let relas = Elf64Relas::new(relas_buf, dynamic_rela.entsize)?;

        let symtab = match &dynamic.symtab {
            Some(dynamic_symtab) => {
                let syms_buf = self.map_vaddr_to_file_buf(dynamic_symtab.base_vaddr, None)?;
                let symtab = Elf64Symtab::new(syms_buf, dynamic_symtab.entsize)?;
                Some(symtab)
            }
            None => None,
        };

        Ok(Some(Elf64AppliedRelaIterator::new(
            rela_proc,
            load_base,
            &self.load_segments,
            relas,
            symtab,
        )))
    }

    /// Retrieves the entry point virtual address of the ELF image.
    ///
    /// This function returns the virtual address of the entry point of the ELF image.
    /// The `image_load_addr` parameter specifies the virtual address where the ELF image
    /// is loaded in memory, and the entry point address is adjusted accordingly.
    ///
    /// # Arguments
    ///
    /// * `image_load_addr` - The virtual address where the ELF image is loaded in memory.
    ///
    /// # Returns
    ///
    /// The adjusted entry point virtual address.
    pub fn get_entry(&self, image_load_addr: Elf64Addr) -> Elf64Addr {
        self.elf_hdr
            .e_entry
            .wrapping_add(self.load_base(image_load_addr))
    }
}

/// Represents an ELF64 string table ([`Elf64Strtab`]) containing strings
/// used within the ELF file
#[derive(Debug, Default, PartialEq)]
struct Elf64Strtab<'a> {
    strtab_buf: &'a [u8],
}

impl<'a> Elf64Strtab<'a> {
    /// Creates a new [`Elf64Strtab`] instance from the provided string table buffer
    fn new(strtab_buf: &'a [u8]) -> Self {
        Self { strtab_buf }
    }

    /// Retrieves a string from the string table by its index.
    ///
    /// # Arguments
    ///
    /// - `index`: The index of the string to retrieve.
    ///
    /// # Returns
    ///
    /// - [`Result<&'a ffi::CStr, ElfError>`]: A [`Result`] containing the string as a CStr reference
    ///   if found, or an [`ElfError`] if the index is out of bounds or the string is invalid.
    #[allow(unused)]
    fn get_str(&self, index: Elf64Word) -> Result<&'a ffi::CStr, ElfError> {
        let index = usize::try_from(index).unwrap();
        if index >= self.strtab_buf.len() {
            return Err(ElfError::InvalidStrtabString);
        }

        ffi::CStr::from_bytes_until_nul(&self.strtab_buf[index..])
            .map_err(|_| ElfError::InvalidStrtabString)
    }
}

/// Represents an ELF64 symbol ([`Elf64Sym`]) within the symbol table.
#[derive(Debug)]
struct Elf64Sym {
    /// Name of the symbol as an index into the string table
    #[allow(unused)]
    st_name: Elf64Word,
    /// Symbol information and binding attributes
    #[allow(unused)]
    st_info: Elf64char,
    /// Reserved for additional symbol attributes (unused)
    #[allow(unused)]
    st_other: Elf64char,
    /// Section index associated with the symbol
    st_shndx: Elf64Half,
    /// Value or address of the symbol
    st_value: Elf64Addr,
    /// Size of the symbol in bytes
    #[allow(unused)]
    st_size: Elf64Xword,
}

impl Elf64Sym {
    /// Reads an [`Elf64Sym`] from the provided buffer.
    ///
    /// # Arguments
    ///
    /// - `buf`: A slice of bytes containing the symbol data.
    ///
    /// # Returns
    ///
    /// - [`Elf64Sym`]: An [`Elf64Sym`] instance parsed from the buffer.
    fn read(buf: &[u8]) -> Self {
        let st_name = Elf64Word::from_le_bytes(buf[0..4].try_into().unwrap());
        let st_info = Elf64char::from_le_bytes(buf[4..5].try_into().unwrap());
        let st_other = Elf64char::from_le_bytes(buf[5..6].try_into().unwrap());
        let st_shndx = Elf64Half::from_le_bytes(buf[6..8].try_into().unwrap());
        let st_value = Elf64Addr::from_le_bytes(buf[8..16].try_into().unwrap());
        let st_size = Elf64Xword::from_le_bytes(buf[16..24].try_into().unwrap());
        Self {
            st_name,
            st_info,
            st_other,
            st_shndx,
            st_value,
            st_size,
        }
    }
}

/// Represents an ELF64 symbol table ([`Elf64Symtab`]) containing
/// symbols used within the ELF file.
#[derive(Debug)]
pub struct Elf64Symtab<'a> {
    /// The underlying buffer containing the symbol table data
    syms_buf: &'a [u8],
    /// Size of each symbol entry in bytes
    entsize: usize,
    /// Number of symbols in the symbol table
    syms_num: Elf64Word,
}

impl<'a> Elf64Symtab<'a> {
    /// Indicates an undefined symbol
    const STN_UNDEF: Elf64Word = 0;

    /// Creates a new [`Elf64Symtab`] instance from the provided symbol table buffer.
    ///
    /// # Arguments
    ///
    /// - `syms_buf`: The buffer containing the symbol table data.
    /// - `entsize`: The size of each symbol entry in bytes.
    ///
    /// # Returns
    ///
    /// - [`Result<Self, ElfError>`]: A [`Result`] containing the [`Elf64Symtab`] instance if valid,
    ///   or an [`ElfError`] if the provided parameters are invalid.
    fn new(syms_buf: &'a [u8], entsize: Elf64Xword) -> Result<Self, ElfError> {
        let entsize = usize::try_from(entsize).map_err(|_| ElfError::InvalidSymbolEntrySize)?;
        if entsize < 24 {
            return Err(ElfError::InvalidSymbolEntrySize);
        }
        let syms_num = syms_buf.len() / entsize;
        let syms_num = Elf64Word::try_from(syms_num).map_err(|_| ElfError::InvalidSymbolIndex)?;
        Ok(Self {
            syms_buf,
            entsize,
            syms_num,
        })
    }

    /// Reads a symbol from the symbol table by its index.
    ///
    /// # Arguments
    ///
    /// - `i`: The index of the symbol to retrieve.
    ///
    /// # Returns
    ///
    /// - [`Result<Elf64Sym, ElfError>`]: A [`Result`] containing the [`Elf64Sym`] if found,
    ///   or an [`ElfError`] if the index is out of bounds or the symbol is invalid.
    fn read_sym(&self, i: Elf64Word) -> Result<Elf64Sym, ElfError> {
        if i > self.syms_num {
            return Err(ElfError::InvalidSymbolIndex);
        }
        let i = usize::try_from(i).map_err(|_| ElfError::InvalidSymbolIndex)?;
        let sym_off = i * self.entsize;
        let sym_buf = &self.syms_buf[sym_off..(sym_off + self.entsize)];
        Ok(Elf64Sym::read(sym_buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_elf64_shdr_verify_methods() {
        // Create a valid Elf64Shdr instance for testing.
        let valid_shdr = Elf64Shdr {
            sh_name: 1,
            sh_type: 2,
            sh_flags: Elf64ShdrFlags::WRITE | Elf64ShdrFlags::ALLOC,
            sh_addr: 0x1000,
            sh_offset: 0x2000,
            sh_size: 0x3000,
            sh_link: 3,
            sh_info: 4,
            sh_addralign: 8,
            sh_entsize: 0,
        };

        // Verify that the valid Elf64Shdr instance passes verification.
        assert!(valid_shdr.verify().is_ok());

        // Create an invalid Elf64Shdr instance for testing.
        let invalid_shdr = Elf64Shdr {
            sh_name: 0,
            sh_type: 2,
            sh_flags: Elf64ShdrFlags::from_bits(0).unwrap(),
            sh_addr: 0x1000,
            sh_offset: 0x2000,
            sh_size: 0x3000,
            sh_link: 3,
            sh_info: 4,
            sh_addralign: 7, // Invalid alignment
            sh_entsize: 0,
        };

        // Verify that the invalid Elf64Shdr instance fails verification.
        assert!(invalid_shdr.verify().is_err());
    }

    #[test]
    fn test_elf64_dynamic_reloc_table_verify_valid() {
        // Create a valid Elf64DynamicRelocTable instance for testing.
        let reloc_table = Elf64DynamicRelocTable {
            base_vaddr: 0x1000,
            size: 0x2000,
            entsize: 0x30,
        };

        // Verify that the valid Elf64DynamicRelocTable instance passes verification.
        assert!(reloc_table.verify().is_ok());
    }

    #[test]
    fn test_elf64_addr_range_methods() {
        // Test Elf64AddrRange::len() and Elf64AddrRange::is_empty().

        // Create an Elf64AddrRange instance for testing.
        let addr_range = Elf64AddrRange {
            vaddr_begin: 0x1000,
            vaddr_end: 0x2000,
        };

        // Check that the length calculation is correct.
        assert_eq!(addr_range.len(), 0x1000);

        // Check if the address range is empty.
        assert!(!addr_range.is_empty());

        // Test Elf64AddrRange::try_from().

        // Create a valid input tuple for try_from.
        let valid_input: (Elf64Addr, Elf64Xword) = (0x1000, 0x2000);

        // Attempt to create an Elf64AddrRange from the valid input.
        let result = Elf64AddrRange::try_from(valid_input);

        // Verify that the result is Ok and contains the expected Elf64AddrRange.
        assert!(result.is_ok());
        let valid_addr_range = result.unwrap();
        assert_eq!(valid_addr_range.vaddr_begin, 0x1000);
        assert_eq!(valid_addr_range.vaddr_end, 0x3000);
    }

    #[test]
    fn test_elf64_file_range_try_from() {
        // Valid range
        let valid_range: (Elf64Off, Elf64Xword) = (0, 100);
        let result: Result<Elf64FileRange, ElfError> = valid_range.try_into();
        assert!(result.is_ok());
        let file_range = result.unwrap();
        assert_eq!(file_range.offset_begin, 0);
        assert_eq!(file_range.offset_end, 100);

        // Invalid range (overflow)
        let invalid_range: (Elf64Off, Elf64Xword) = (usize::MAX as Elf64Off, 100);
        let result: Result<Elf64FileRange, ElfError> = invalid_range.try_into();
        assert!(result.is_err());
    }

    #[test]
    fn test_elf64_file_read() {
        // In the future, you can play around with this skeleton ELF
        // file to test other cases
        let byte_data: [u8; 184] = [
            // ELF Header
            0x7F, 0x45, 0x4C, 0x46, 0x02, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x02, 0x00, 0x3E, 0x00, 0x01, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, // Program Header (with PT_LOAD)
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, // Section Header (simplified)
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, // Raw Machine Code Instructions
            0xf3, 0x0f, 0x1e, 0xfa, 0x31, 0xed, 0x49, 0x89, 0xd1, 0x5e, 0x48, 0x89, 0xe2, 0x48,
            0x83, 0xe4, 0xf0, 0x50, 0x54, 0x45, 0x31, 0xc0, 0x31, 0xc9, 0x48, 0x8d, 0x3d, 0xca,
            0x00, 0x00, 0x00, 0xff, 0x15, 0x53, 0x2f, 0x00, 0x00, 0xf4, 0x66, 0x2e, 0x0f, 0x1f,
            0x84, 0x00, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x3d, 0x79, 0x2f, 0x00, 0x00, 0x48, 0x8d,
            0x05, 0x72, 0x2f, 0x00, 0x00, 0x48, 0x39, 0xf8, 0x74, 0x15, 0x48, 0x8b, 0x05, 0x36,
            0x2f, 0x00, 0x00, 0x48, 0x85, 0xc0, 0x74, 0x09, 0xff, 0xe0, 0x0f, 0x1f, 0x80, 0x00,
            0x00, 0x00, 0x00, 0xc3,
        ];

        // Use the Elf64File::read method to create an Elf64File instance
        let res = Elf64File::read(&byte_data);
        assert_eq!(res, Err(ElfError::InvalidPhdrSize));

        // Construct an Elf64Hdr instance from the byte data
        let elf_hdr = Elf64Hdr::read(&byte_data);

        // Did we fail to read the ELF header?
        assert!(elf_hdr.is_ok());
        let elf_hdr = elf_hdr.unwrap();

        let expected_type = 2;
        let expected_machine = 0x3E;
        let expected_version = 1;

        // Assert that the fields of the header match the expected values
        assert_eq!(elf_hdr.e_type, expected_type);
        assert_eq!(elf_hdr.e_machine, expected_machine);
        assert_eq!(elf_hdr.e_version, expected_version);
    }

    #[test]
    fn test_elf64_load_segments() {
        let mut load_segments = Elf64LoadSegments::new();
        let vaddr_range1 = Elf64AddrRange {
            vaddr_begin: 0x1000,
            vaddr_end: 0x2000,
        };
        let vaddr_range2 = Elf64AddrRange {
            vaddr_begin: 0x3000,
            vaddr_end: 0x4000,
        };
        let segment_index1 = 0;
        let segment_index2 = 1;

        // Insert load segments
        assert!(load_segments
            .try_insert(vaddr_range1, segment_index1)
            .is_ok());
        assert!(load_segments
            .try_insert(vaddr_range2, segment_index2)
            .is_ok());

        // Lookup load segments by virtual address
        let (index1, offset1) = load_segments
            .lookup_vaddr_range(&Elf64AddrRange {
                vaddr_begin: 0x1500,
                vaddr_end: 0x1700,
            })
            .unwrap();
        let (index2, offset2) = load_segments
            .lookup_vaddr_range(&Elf64AddrRange {
                vaddr_begin: 0x3500,
                vaddr_end: 0x3700,
            })
            .unwrap();

        assert_eq!(index1, segment_index1);
        assert_eq!(offset1, 0x500); // Offset within the segment
        assert_eq!(index2, segment_index2);
        assert_eq!(offset2, 0x500); // Offset within the segment

        // Total virtual address range
        let total_range = load_segments.total_vaddr_range();
        assert_eq!(total_range.vaddr_begin, 0x1000);
        assert_eq!(total_range.vaddr_end, 0x4000);
    }
}
