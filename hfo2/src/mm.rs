/*
 * Copyright 2019 Jeehoon Kang
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! # Memory management via page tables.
//!
//! This file has functions for managing the level 1 and 2 page tables used by Hafnium.  There is a
//! level 1 mapping used by Hafnium itself to access memory, and then a level 2 mapping per VM.  The
//! design assumes that all page tables contain only 1-1 mappings, aligned on the block boundaries.
//!
//! ## Assumptions
//!
//! We assume that the stage 1 and stage 2 page table addresses are `usize`.  It looks like that
//! assumption might not be holding so we need to check that everything is going to be okay.

use core::cmp;
use core::marker::PhantomData;
use core::mem;
use core::ops::*;
use core::ptr;
use core::slice;
use core::sync::atomic::{fence, AtomicBool, Ordering};
use reduce::Reduce;

use crate::mpool::MPool;
use crate::page::*;
use crate::spinlock::SpinLock;
use crate::types::*;
use crate::utils::*;

extern "C" {
    fn arch_mm_absent_pte(level: u8) -> usize;
    fn arch_mm_table_pte(level: u8, pa: usize) -> usize;
    fn arch_mm_block_pte(level: u8, pa: usize, attrs: usize) -> usize;

    fn arch_mm_is_block_allowed(level: u8) -> bool;
    fn arch_mm_pte_is_present(pte: usize, level: u8) -> bool;
    fn arch_mm_pte_is_valid(pte: usize, level: u8) -> bool;
    fn arch_mm_pte_is_block(pte: usize, level: u8) -> bool;
    fn arch_mm_pte_is_table(pte: usize, level: u8) -> bool;

    fn arch_mm_clear_pa(pa: usize) -> usize;
    fn arch_mm_block_from_pte(pte: usize, level: u8) -> usize;
    fn arch_mm_table_from_pte(pte: usize, level: u8) -> usize;
    fn arch_mm_pte_attrs(pte: usize, level: u8) -> usize;

    fn arch_mm_invalidate_stage1_range(begin: usize, end: usize);
    fn arch_mm_invalidate_stage2_range(begin: usize, end: usize);

    fn arch_mm_mode_to_stage1_attrs(mode: c_int) -> usize;
    fn arch_mm_mode_to_stage2_attrs(mode: c_int) -> usize;

    fn arch_mm_stage2_attrs_to_mode(attrs: usize) -> c_int;

    fn arch_mm_stage1_max_level() -> u8;
    fn arch_mm_stage2_max_level() -> u8;

    fn arch_mm_stage1_root_table_count() -> u8;
    fn arch_mm_stage2_root_table_count() -> u8;

    fn arch_mm_init(table: usize, first: bool) -> bool;

    fn arch_mm_combine_table_entry_attrs(table_attrs: usize, block_attrs: usize) -> usize;

    fn plat_console_mm_init(mpool: *const MPool);

    fn layout_text_begin() -> usize;
    fn layout_text_end() -> usize;
    fn layout_rodata_begin() -> usize;
    fn layout_rodata_end() -> usize;
    fn layout_data_begin() -> usize;
    fn layout_data_end() -> usize;
}

bitflags! {
    /// Arch-independent page mapping modes.
    ///
    /// Memory in stage-1 is either valid (present) or invalid (absent).
    ///
    /// Memory in stage-2 has more states to track sharing, borrowing and giving of memory. The
    /// states are made up of three parts:
    ///
    ///  1. V = valid/invalid    : Whether the memory is part of the VM's address
    ///                            space. A fault will be generated if accessed when
    ///                            invalid.
    ///  2. O = owned/unowned    : Whether the memory is owned by the VM.
    ///  3. X = exclusive/shared : Whether access is exclusive to the VM or shared
    ///                            with at most one other.
    ///
    /// These parts compose to form the following state:
    ///
    ///  -  V  O  X : Owner of memory with exclusive access.
    ///  -  V  O !X : Owner of memory with access shared with at most one other VM.
    ///  -  V !O  X : Borrower of memory with exclusive access.
    ///  -  V !O !X : Borrower of memory where access is shared with the owner.
    ///  - !V  O  X : Owner of memory lent to a VM that has exclusive access.
    ///
    ///  - !V  O !X : Unused. Owner of shared memory always has access.
    ///
    ///  - !V !O  X : Invalid memory. Memory is unrelated to the VM.
    ///  - !V !O !X : Invalid memory. Memory is unrelated to the VM.
    ///
    ///  Modes are selected so that owner of exclusive memory is the default.
    pub struct Mode: u32 {
        /// Read
        const R       = 0b00000001;

        /// Write
        const W       = 0b00000010;

        /// Execute
        const X       = 0b00000100;

        /// Device
        const D       = 0b00001000;

        /// Invalid
        const INVALID = 0b00010000;

        /// Unowned
        const UNOWNED = 0b00100000;

        /// Shared
        const SHARED  = 0b01000000;
    }
}

bitflags! {
    /// Flags for memory management operations.
    struct Flags: u32 {
        /// Commit
        const COMMIT = 0b01;

        /// Unmap
        const UNMAP  = 0b10;
    }
}

/// The hypervisor page table.
pub static HYPERVISOR_PAGE_TABLE: SpinLock<PageTable<Stage1>> =
    SpinLock::new(unsafe { PageTable::null() });

/// Is stage2 invalidation enabled?
pub static STAGE2_INVALIDATE: AtomicBool = AtomicBool::new(false);

/// Utility functions for address manipulation.
mod addr {
    use crate::page::*;

    /// Rounds an address down to a page boundary.
    pub fn round_down_to_page(addr: usize) -> usize {
        addr & !(PAGE_SIZE - 1)
    }

    /// Rounds an address up to a page boundary.
    pub fn round_up_to_page(addr: usize) -> usize {
        round_down_to_page(addr + PAGE_SIZE - 1)
    }

    /// Calculates the size of the address space represented by a page table entry at the given
    /// level.
    pub fn entry_size(level: u8) -> usize {
        1usize << (PAGE_BITS + level as usize * PAGE_LEVEL_BITS)
    }

    /// Gets the address of the start of the next block of the given size. The size must be a power
    /// of two.
    pub fn start_of_next_block(addr: usize, block_size: usize) -> usize {
        (addr + block_size) & !(block_size - 1)
    }

    /// For a given address, calculates the maximum (plus one) address that can be represented by
    /// the same table at the given level.
    pub fn level_end(addr: usize, level: u8) -> usize {
        let offset = PAGE_BITS + (level as usize + 1) * PAGE_LEVEL_BITS;
        ((addr >> offset) + 1) << offset
    }

    /// For a given address, calculates the index at which its entry is stored in a table at the
    /// given level.
    pub fn index(addr: usize, level: u8) -> usize {
        let v = addr >> (PAGE_BITS + level as usize * PAGE_LEVEL_BITS);
        v & ((1usize << PAGE_LEVEL_BITS) - 1)
    }
}

/// Page table stage.
pub trait Stage {
    /// Returns the maximum level in the page table.
    fn max_level() -> u8;

    /// Returns the number of root-level tables.
    fn root_table_count() -> u8;

    /// Invalidates the TLB for the given address range.
    fn invalidate_tlb(begin: usize, end: usize);

    /// Converts the mode into attributes for a block PTE.
    fn mode_to_attrs(mode: Mode) -> usize;

    /// Converts the attributes back to the corresponding mode.
    fn attrs_to_mode(attrs: usize) -> Mode;
}

/// The page table stage for the hypervisor.
pub struct Stage1 {}

impl Stage for Stage1 {
    fn max_level() -> u8 {
        unsafe { arch_mm_stage1_max_level() }
    }

    fn root_table_count() -> u8 {
        unsafe { arch_mm_stage1_root_table_count() }
    }

    fn invalidate_tlb(begin: usize, end: usize) {
        unsafe {
            arch_mm_invalidate_stage1_range(begin, end);
        }
    }

    fn mode_to_attrs(mode: Mode) -> usize {
        unsafe { arch_mm_mode_to_stage1_attrs(mode.bits as c_int) }
    }

    fn attrs_to_mode(_attrs: usize) -> Mode {
        panic!("`arch_mm_stage2_attrs_to_mode()` doesn't exist");
    }
}

/// The page table stage for VMs.
pub struct Stage2 {}

impl Stage for Stage2 {
    fn max_level() -> u8 {
        unsafe { arch_mm_stage2_max_level() }
    }

    fn root_table_count() -> u8 {
        unsafe { arch_mm_stage2_root_table_count() }
    }

    fn invalidate_tlb(begin: usize, end: usize) {
        if STAGE2_INVALIDATE.load(Ordering::Relaxed) {
            unsafe {
                arch_mm_invalidate_stage2_range(begin, end);
            }
        }
    }

    fn mode_to_attrs(mode: Mode) -> usize {
        unsafe { arch_mm_mode_to_stage2_attrs(mode.bits as c_int) }
    }

    fn attrs_to_mode(attrs: usize) -> Mode {
        Mode::from_bits_truncate(unsafe { arch_mm_stage2_attrs_to_mode(attrs) } as u32)
    }
}

/// Page table entry.
#[repr(C)]
struct PageTableEntry {
    inner: usize,
}

impl PageTableEntry {
    /// Creates a page table entry from the inner representation.
    ///
    /// # Safety
    ///
    /// Improper use of this function may lead to memory problems.  For example, a double-free may
    /// occur if the function is called twice on the same raw pointer.
    unsafe fn from_raw(inner: usize) -> Self {
        Self { inner }
    }

    fn absent(level: u8) -> Self {
        unsafe { Self::from_raw(arch_mm_absent_pte(level)) }
    }

    fn block(level: u8, begin: usize, attrs: usize) -> Self {
        unsafe { Self::from_raw(arch_mm_block_pte(level, begin, attrs)) }
    }

    /// # Safety
    ///
    /// `page` should be a proper page table.
    unsafe fn table(level: u8, page: Page) -> Self {
        Self::from_raw(arch_mm_table_pte(level, page.into_raw() as usize))
    }

    fn is_present(&self, level: u8) -> bool {
        unsafe { arch_mm_pte_is_present(self.inner, level) }
    }

    fn is_valid(&self, level: u8) -> bool {
        unsafe { arch_mm_pte_is_valid(self.inner, level) }
    }

    fn is_block(&self, level: u8) -> bool {
        unsafe { arch_mm_pte_is_block(self.inner, level) }
    }

    fn is_table(&self, level: u8) -> bool {
        unsafe { arch_mm_pte_is_table(self.inner, level) }
    }

    fn attrs(&self, level: u8) -> usize {
        unsafe { arch_mm_pte_attrs(self.inner, level) }
    }

    fn as_block(&self, level: u8) -> Option<usize> {
        if self.is_block(level) {
            Some(unsafe { self.as_block_unchecked(level) })
        } else {
            None
        }
    }

    unsafe fn as_block_unchecked(&self, level: u8) -> usize {
        arch_mm_block_from_pte(self.inner, level)
    }

    fn as_table(&self, level: u8) -> Option<&RawPageTable> {
        if self.is_table(level) {
            unsafe { Some(&*(arch_mm_table_from_pte(self.inner, level) as *const _)) }
        } else {
            None
        }
    }

    fn as_table_mut(&mut self, level: u8) -> Option<&mut RawPageTable> {
        unsafe {
            if arch_mm_pte_is_table(self.inner, level) {
                Some(&mut *(arch_mm_table_from_pte(self.inner, level) as *mut _))
            } else {
                None
            }
        }
    }

    /// Frees all page-table-related memory associated with the given pte at the given level,
    /// including any subtables recursively.
    ///
    /// # Safety
    ///
    /// After a page table entry is freed, it's value is undefined.
    unsafe fn free(&mut self, level: u8, mpool: &MPool) {
        if let Some(table) = self.as_table_mut(level) {
            // Recursively free any subtables.
            for pte in table.iter_mut() {
                pte.free(level - 1, mpool);
            }

            // Free the table itself.
            mpool.free(Page::from_raw(table as *mut _ as *mut _));
        }
    }

    /// Replaces a page table entry with the given value. If both old and new values are valid, it
    /// performs a break-before-make sequence where it first writes an invalid value to the PTE,
    /// flushes the TLB, then writes the actual new value.  This is to prevent cases where CPUs have
    /// different 'valid' values in their TLBs, which may result in issues for example in cache
    /// coherency.
    fn replace<S: Stage>(
        &mut self,
        new_pte: PageTableEntry,
        begin: usize,
        level: u8,
        mpool: &MPool,
    ) {
        let inner = self.inner;

        // We need to do the break-before-make sequence if both values are present and the TLB is
        // being invalidated.
        if self.is_valid(level) && new_pte.is_valid(level) {
            unsafe { ptr::write(self, Self::absent(level)) };
            S::invalidate_tlb(begin, begin + addr::entry_size(level));
        }

        // Assign the new pte.
        unsafe {
            ptr::write(self, new_pte);
        }

        // Free pages that aren't in use anymore.
        unsafe {
            let mut old_pte = Self::from_raw(inner);
            old_pte.free(level, mpool);
            mem::forget(old_pte);
        }
    }

    /// Populates the provided page table entry with a reference to another table if needed, that
    /// is, if it does not yet point to another table.
    ///
    /// Returns a pointer to the table the entry now points to.
    fn populate_table<S: Stage>(&mut self, begin: usize, level: u8, mpool: &MPool) -> Option<()> {
        // Just return if it's already populated.
        if self.is_table(level) {
            return Some(());
        }

        // Allocate a new table.
        let mut page = mpool
            .alloc()
            .ok_or_else(|| dlog!("Failed to allocate memory for page table\n"))
            .ok()?;

        let table = unsafe { RawPageTable::deref_mut_page(&mut page) };

        // Initialise entries in the new table.
        let level_below = level - 1;
        if self.is_block(level) {
            let attrs = self.attrs(level);
            let entry_size = addr::entry_size(level_below);

            for (i, pte) in table.iter_mut().enumerate() {
                unsafe {
                    ptr::write(
                        pte,
                        Self::block(level_below, self.inner + i * entry_size, attrs),
                    );
                }
            }
        } else {
            for pte in table.iter_mut() {
                unsafe { ptr::write(pte, Self::absent(level_below)) };
            }
        }

        // Ensure initialisation is visible before updating the pte.
        //
        // TODO(@jeehoonkang): very suspicious..
        fence(Ordering::Release);

        // Replace the pte entry, doing a break-before-make if needed.
        let table = unsafe { Self::table(level, page) };
        self.replace::<S>(table, begin, level, mpool);

        Some(())
    }

    /// Defragments the given PTE by recursively replacing any tables with blocks or absent entries
    /// where possible.
    fn defrag(&mut self, level: u8, mpool: &MPool) -> Option<usize> {
        let attrs = self.attrs(level);

        if self.is_block(level) {
            return Some(attrs);
        }

        let table = self.as_table_mut(level)?;

        // First try to defrag the entry, in case it is a subtable. Then check if all entries are
        // blocks with the same flags or are all absent. It assumes addresses are contiguous due to
        // identity mapping.
        let children_attrs = table
            .iter_mut()
            .map(|pte| pte.defrag(level - 1, mpool))
            .reduce(|l, r| if l == r { l } else { None })??;

        // If the table's all the entries are absent, free the table and return an absent entry.
        unsafe {
            if !arch_mm_pte_is_present(children_attrs, level - 1) {
                mpool.free(Page::from_raw(table as *mut _ as *mut _));
                ptr::write(self, Self::absent(level));
                return Some(self.attrs(level));
            }
        }

        // Bail out if block is not allowed in the current level.
        if unsafe { !arch_mm_is_block_allowed(level) } {
            return None;
        }

        // Merge table into a single block with equivalent attributes.
        let block_address = unsafe { table.get_unchecked(0).as_block_unchecked(level - 1) };
        let combined_attrs = unsafe { arch_mm_combine_table_entry_attrs(attrs, children_attrs) };

        mpool.free(unsafe { Page::from_raw(table as *mut _ as *mut _) });
        unsafe {
            ptr::write(
                self,
                PageTableEntry::block(level, block_address, combined_attrs),
            );
        }

        Some(combined_attrs)
    }
}

impl Drop for PageTableEntry {
    fn drop(&mut self) {
        panic!("`PageEntry` should not be dropped.");
    }
}

struct BlockIter {
    begin: usize,
    end: usize,
    block_size: usize,
}

impl BlockIter {
    const fn new(begin: usize, end: usize, block_size: usize) -> Self {
        Self {
            begin,
            end,
            block_size,
        }
    }
}

impl Iterator for BlockIter {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.begin >= self.end {
            return None;
        }

        let result = self.begin;
        self.begin = addr::start_of_next_block(self.begin, self.block_size);
        Some(result)
    }
}

/// Number of page table entries in a page table.
pub const PTE_PER_PAGE: usize = (PAGE_SIZE / mem::size_of::<PageTableEntry>());

#[repr(align(4096))]
struct RawPageTable {
    entries: [PageTableEntry; PTE_PER_PAGE],
}

const_assert!(raw_page_table_align; mem::align_of::<RawPageTable>() == PAGE_SIZE);
const_assert!(raw_page_table_size; mem::size_of::<RawPageTable>() == PAGE_SIZE);

impl Deref for RawPageTable {
    type Target = [PageTableEntry; PTE_PER_PAGE];

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl DerefMut for RawPageTable {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entries
    }
}

impl RawPageTable {
    unsafe fn deref_page(page: &Page) -> &Self {
        Self::deref_raw_page(page)
    }

    unsafe fn deref_mut_page(page: &mut Page) -> &mut Self {
        Self::deref_mut_raw_page(page)
    }

    unsafe fn deref_raw_page(page: &RawPage) -> &Self {
        &*(page as *const _ as *const _)
    }

    unsafe fn deref_mut_raw_page(page: &mut RawPage) -> &mut Self {
        &mut *(page as *mut _ as *mut _)
    }

    /// Returns whether all entries in this table are absent.
    fn is_empty(&self, level: u8) -> bool {
        self.iter().all(|pte| !pte.is_present(level))
    }

    /// Updates the page table at the given level to map the given address range to a physical range
    /// using the provided (architecture-specific) attributes. Or if MM_FLAG_UNMAP is set, unmap the
    /// given range instead.
    ///
    /// This function calls itself recursively if it needs to update additional levels, but the
    /// recursion is bound by the maximum number of levels in a page table.
    fn map_level<S: Stage>(
        &mut self,
        begin: usize,
        end: usize,
        attrs: usize,
        level: u8,
        flags: Flags,
        mpool: &MPool,
    ) -> Option<()> {
        let entry_size = addr::entry_size(level);
        let commit = !(flags & Flags::COMMIT).is_empty();
        let unmap = !(flags & Flags::UNMAP).is_empty();

        let ptes = self[addr::index(begin, level)..].iter_mut();
        let begins = BlockIter::new(
            begin,
            // Cap end so that we don't go over the current level max.
            cmp::min(end, addr::level_end(begin, level)),
            entry_size,
        );

        // Fill each entry in the table.
        for (pte, begin) in ptes.zip(begins) {
            // If the entry is already mapped with the right attributes, or already absent in the
            // case of unmapping, no need to do anything; carry on to the next entry.
            if unmap && !pte.is_present(level) {
                continue;
            }
            if !unmap && pte.is_block(level) && pte.attrs(level) == attrs {
                continue;
            }

            // If the entire entry is within the region we want to map, map/unmap the whole entry.
            if end - begin >= entry_size
                && (unmap || unsafe { arch_mm_is_block_allowed(level) })
                && (begin & (entry_size - 1) == 0)
            {
                if commit {
                    let new_pte = if unmap {
                        PageTableEntry::absent(level)
                    } else {
                        PageTableEntry::block(level, begin, attrs)
                    };
                    pte.replace::<S>(new_pte, begin, level, mpool);
                }

                continue;
            }

            // If the entry is already a subtable get it; otherwise replace it with an equivalent
            // subtable and get that.
            pte.populate_table::<S>(begin, level, mpool)?;

            // Since `pte` is just populated, it should be a table.
            let new_table = pte.as_table_mut(level).unwrap();

            // Recurse to map/unmap the appropriate entries within the subtable.
            new_table.map_level::<S>(begin, end, attrs, level - 1, flags, mpool)?;

            // If the subtable is now empty, replace it with an absent entry at this level. We never
            // need to do break-before-makes here because we are assigning an absent value.
            //
            // TODO(@jeehoonkang): I think we should do break-before-makes here due to reordering.
            if commit && unmap && new_table.is_empty(level - 1) {
                pte.replace::<S>(PageTableEntry::absent(level), begin, level, mpool);
            }
        }

        Some(())
    }

    /// Gets the attributes applied to the given range of stage-2 addresses at the given level.
    ///
    /// The `got_attrs` argument is initially passed as false until `attrs` contains attributes of
    /// the memory region at which point it is passed as true.
    ///
    /// The value returned in `attrs` is only valid if the function returns true.
    ///
    /// Returns true if the whole range has the same attributes and false otherwise.
    pub fn get_attrs_level(&self, begin: usize, end: usize, level: u8) -> Option<usize> {
        let ptes = self[addr::index(begin, level)..].iter();
        let begins = BlockIter::new(
            begin,
            // Cap end so that we don't go over the current level max.
            cmp::min(end, addr::level_end(begin, level)),
            addr::entry_size(level),
        );

        // Check that each entry is owned.
        ptes.zip(begins)
            .map(|(pte, begin)| {
                if let Some(table) = pte.as_table(level) {
                    table.get_attrs_level(begin, end, level - 1)
                } else {
                    Some(pte.attrs(level))
                }
            })
            .opt_reduce(|l, r| if l == r { Some(l) } else { None })
    }

    /// Writes the given table to the debug log, calling itself recursively to write sub-tables.
    fn dump(&self, level: u8, max_level: u8) {
        for (i, pte) in self.iter().enumerate() {
            if !pte.is_present(level) {
                continue;
            }

            dlog!(
                "%{:width$}{:#x}: {}\n",
                "",
                i,
                pte.inner,
                width = (4 * (max_level - level) as usize)
            );

            if let Some(table) = pte.as_table(level) {
                table.dump(level - 1, max_level);
            }
        }
    }
}

/// Page table.
pub struct PageTable<S: Stage> {
    root: usize,
    _marker: PhantomData<S>,
}

impl<S: Stage> PageTable<S> {
    const unsafe fn from_raw(root: usize) -> Self {
        Self {
            root,
            _marker: PhantomData,
        }
    }

    const unsafe fn null() -> Self {
        Self::from_raw(0)
    }

    /// Creates a new page table.
    pub fn new(mpool: &MPool) -> Option<Self> {
        let root_table_count = S::root_table_count();
        let mut pages = mpool.alloc_pages(root_table_count as usize, root_table_count as usize)?;

        for page in pages.iter_mut() {
            let table = unsafe { RawPageTable::deref_mut_raw_page(page) };

            for pte in table.iter_mut() {
                unsafe { ptr::write(pte, PageTableEntry::absent(S::max_level())) };
            }
        }

        // TODO: halloc could return a virtual or physical address if mm not enabled?
        Some(Self {
            root: pages.into_raw() as usize,
            _marker: PhantomData,
        })
    }

    /// Frees all memory associated with the give page table.
    pub fn drop(mut self, mpool: &MPool) {
        let level = S::max_level();

        for page_table in self.deref_mut().iter_mut() {
            for pte in page_table.iter_mut() {
                unsafe {
                    pte.free(level, mpool);
                }
            }
        }

        mpool.free_pages(unsafe {
            Pages::from_raw(self.root as *mut _, S::root_table_count() as usize)
        });
        mem::forget(self);
    }

    fn get_raw(&self) -> *const RawPage {
        self.root as *const RawPage
    }

    fn deref(&self) -> &[RawPageTable] {
        unsafe {
            slice::from_raw_parts(
                self.root as *const RawPageTable,
                S::root_table_count() as usize,
            )
        }
    }

    fn deref_mut(&mut self) -> &mut [RawPageTable] {
        unsafe {
            slice::from_raw_parts_mut(
                self.root as *mut RawPageTable,
                S::root_table_count() as usize,
            )
        }
    }

    /// Updates the page table from the root to map the given address range to a physical range
    /// using the provided (architecture-specific) attributes. Or if MM_FLAG_UNMAP is set, unmap the
    /// given range instead.
    fn map_root(
        &mut self,
        begin: usize,
        end: usize,
        attrs: usize,
        root_level: u8,
        flags: Flags,
        mpool: &MPool,
    ) -> Option<()> {
        let root_table_size = addr::entry_size(root_level);

        let tables = self.deref_mut()[addr::index(begin, root_level)..].iter_mut();
        let begins = BlockIter::new(begin, end, root_table_size);

        for (table, begin) in tables.zip(begins) {
            table.map_level::<S>(begin, end, attrs, root_level - 1, flags, mpool)?;
        }

        Some(())
    }

    /// Updates the given table such that the given physical address range is mapped or not mapped
    /// into the address space with the architecture-agnostic mode provided.
    fn identity_update(
        &mut self,
        begin: usize,
        end: usize,
        attrs: usize,
        flags: Flags,
        mpool: &MPool,
    ) -> Option<()> {
        let root_level = S::max_level() + 1;
        let ptable_end = S::root_table_count() as usize * addr::entry_size(root_level);
        let end = cmp::min(addr::round_up_to_page(end), ptable_end);
        let begin = unsafe { arch_mm_clear_pa(begin) };

        // Do it in two steps to prevent leaving the table in a halfway updated state. In such a
        // two-step implementation, the table may be left with extra internal tables, but no
        // different mapping on failure.
        self.map_root(begin, end, attrs, root_level, flags, mpool)?;
        self.map_root(begin, end, attrs, root_level, flags | Flags::COMMIT, mpool)?;

        // Invalidate the tlb.
        S::invalidate_tlb(begin, end);

        Some(())
    }

    /// Writes the given table to the debug log.
    pub fn dump(&self) {
        let max_level = S::max_level();

        for table in self.deref().iter() {
            table.dump(max_level, max_level);
        }
    }

    /// Defragments the given page table by converting page table references to blocks whenever
    /// possible.
    pub fn defrag(&mut self, mpool: &MPool) {
        let level = S::max_level();

        // Loop through each entry in the table. If it points to another table, check if that table
        // can be replaced by a block or an absent entry.
        for page_table in self.deref_mut().iter_mut() {
            for pte in page_table.iter_mut() {
                pte.defrag(level, mpool);
            }
        }
    }

    pub fn identity_map(
        &mut self,
        begin: usize,
        end: usize,
        mode: Mode,
        mpool: &MPool,
    ) -> Option<()> {
        self.identity_update(begin, end, S::mode_to_attrs(mode), Flags::empty(), mpool)
    }

    /// nUpdates the VM's table such that the given physical address range has no connection to the
    /// VM.
    pub fn unmap(&mut self, begin: usize, end: usize, mpool: &MPool) -> Option<()> {
        self.identity_update(
            begin,
            end,
            S::mode_to_attrs(Mode::UNOWNED | Mode::INVALID | Mode::SHARED),
            Flags::UNMAP,
            mpool,
        )
    }

    /// Gets the attributes applies to the given range of addresses in the stage-2 table.
    ///
    /// The value returned in `attrs` is only valid if the function returns true.
    ///
    /// Returns true if the whole range has the same attributes and false otherwise.
    pub fn get_attrs(&self, begin: usize, end: usize) -> Option<usize> {
        let max_level = S::max_level();
        let root_level = max_level + 1;
        let root_table_size = addr::entry_size(root_level);
        let ptable_end = S::root_table_count() as usize * root_table_size;

        let begin = addr::round_down_to_page(begin);
        let end = addr::round_up_to_page(end);

        // Fail if the addresses are out of range.
        if !(begin <= end && end <= ptable_end) {
            return None;
        }

        let tables = self.deref()[addr::index(begin, root_level)..].iter();
        let begins = BlockIter::new(begin, end, root_table_size);

        tables
            .zip(begins)
            .map(|(table, begin)| table.get_attrs_level(begin, end, max_level))
            .opt_reduce(|l, r| if l == r { Some(l) } else { None })
    }

    /// Gets the mode of the give range of intermediate physical addresses if they are mapped with
    /// the same mode.
    ///
    /// Returns true if the range is mapped with the same mode and false otherwise.}
    pub fn get_mode(&self, begin: usize, end: usize) -> Option<Mode> {
        let attrs = self.get_attrs(begin, end)?;
        Some(S::attrs_to_mode(attrs))
    }
}

impl<S: Stage> Drop for PageTable<S> {
    fn drop(&mut self) {
        panic!("`PageTable` should not be dropped.");
    }
}

/// After calling this function, modifications to stage-2 page tables will use break-before-make and
/// invalidate the TLB for the affected range.
///
/// # Safety
///
/// This function should not be invoked concurrently with other memory management functions.
#[no_mangle]
pub unsafe extern "C" fn mm_vm_enable_invalidation() {
    STAGE2_INVALIDATE.store(true, Ordering::Relaxed);
}

#[no_mangle]
pub unsafe extern "C" fn mm_vm_init(t: *mut PageTable<Stage2>, mpool: *const MPool) -> bool {
    let mpool = &*mpool;
    PageTable::new(mpool)
        .map(|table| ptr::write(t, table))
        .is_some()
}

#[no_mangle]
pub unsafe extern "C" fn mm_vm_fini(t: *mut PageTable<Stage2>, mpool: *const MPool) {
    let t = PageTable::<Stage2>::from_raw((*t).root);
    let mpool = &*mpool;
    t.drop(mpool);
}

#[no_mangle]
pub unsafe extern "C" fn mm_vm_identity_map(
    t: *mut PageTable<Stage2>,
    begin: usize,
    end: usize,
    mode: c_int,
    ipa: *mut usize,
    mpool: *const MPool,
) -> bool {
    let t = &mut *t;
    let mode = Mode::from_bits_truncate(mode as u32);
    let mpool = &*mpool;
    t.identity_map(begin, end, mode, mpool)
        .map(|_| {
            if !ipa.is_null() {
                ptr::write(ipa, begin);
            }
        })
        .is_some()
}

#[no_mangle]
pub unsafe extern "C" fn mm_vm_unmap(
    t: *mut PageTable<Stage2>,
    begin: usize,
    end: usize,
    mpool: *const MPool,
) -> bool {
    let t = &mut *t;
    let mpool = &*mpool;
    t.identity_update(
        begin,
        end,
        Stage2::mode_to_attrs(Mode::UNOWNED | Mode::INVALID | Mode::SHARED),
        Flags::UNMAP,
        mpool,
    )
    .is_some()
}

#[no_mangle]
pub unsafe extern "C" fn mm_vm_unmap_hypervisor(
    t: *mut PageTable<Stage2>,
    mpool: *const MPool,
) -> bool {
    // TODO: If we add pages dynamically, they must be included here too.
    let t = &mut *t;
    let mpool = &*mpool;
    some_or_return!(
        t.unmap(layout_text_begin(), layout_text_end(), mpool),
        false
    );
    some_or_return!(
        t.unmap(layout_rodata_begin(), layout_rodata_end(), mpool),
        false
    );
    some_or_return!(
        t.unmap(layout_data_begin(), layout_data_end(), mpool),
        false
    );
    true
}

#[no_mangle]
pub unsafe extern "C" fn mm_vm_dump(t: *mut PageTable<Stage2>) {
    let t = &mut *t;
    t.dump();
}

#[no_mangle]
pub unsafe extern "C" fn mm_vm_defrag(t: *mut PageTable<Stage2>, mpool: *const MPool) {
    let t = &mut *t;
    let mpool = &*mpool;
    t.defrag(mpool);
}

#[no_mangle]
pub unsafe extern "C" fn mm_vm_get_mode(
    t: *mut PageTable<Stage2>,
    begin: usize,
    end: usize,
    mode: *mut c_int,
) -> bool {
    let t = &mut *t;
    t.get_mode(begin, end)
        .map(|m| *mode = m.bits as c_int)
        .is_some()
}

#[no_mangle]
pub unsafe extern "C" fn mm_identity_map(
    begin: usize,
    end: usize,
    mode: c_int,
    mpool: *const MPool,
) -> *mut usize {
    let mode = Mode::from_bits_truncate(mode as u32);
    let mpool = &*mpool;
    HYPERVISOR_PAGE_TABLE
        .lock()
        .identity_map(begin, end, mode, mpool)
        .map(|_| begin as *mut _)
        .unwrap_or_else(|| ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn mm_unmap(begin: usize, end: usize, mpool: *const MPool) -> bool {
    let mpool = &*mpool;
    HYPERVISOR_PAGE_TABLE
        .lock()
        .unmap(begin, end, mpool)
        .is_some()
}

#[no_mangle]
pub unsafe extern "C" fn mm_init(mpool: *const MPool) -> bool {
    dlog!(
        "text: {:#x} - {:#x}\n",
        layout_text_begin(),
        layout_text_end()
    );
    dlog!(
        "rodata: {:#x} - {:#x}\n",
        layout_rodata_begin(),
        layout_rodata_end()
    );
    dlog!(
        "data: {:#x} - {:#x}\n",
        layout_data_begin(),
        layout_data_end()
    );

    let mpool = &*mpool;
    let page_table = PageTable::new(mpool)
        .ok_or_else(|| dlog!("Unable to allocate memory for page table.\n"))
        .ok();
    let page_table = some_or_return!(page_table, false);
    let hypervisor_page_table = HYPERVISOR_PAGE_TABLE.get_mut_unchecked();
    ptr::write(hypervisor_page_table, page_table);

    // Let console driver map pages for itself.
    plat_console_mm_init(mpool);

    hypervisor_page_table.identity_map(layout_text_begin(), layout_text_end(), Mode::X, mpool);
    hypervisor_page_table.identity_map(layout_rodata_begin(), layout_rodata_end(), Mode::R, mpool);
    hypervisor_page_table.identity_map(
        layout_data_begin(),
        layout_data_end(),
        Mode::R | Mode::W,
        mpool,
    );

    arch_mm_init(hypervisor_page_table.root, true)
}

#[no_mangle]
pub unsafe extern "C" fn mm_cpu_init() -> bool {
    let ptable = HYPERVISOR_PAGE_TABLE.get_mut_unchecked().get_raw();
    arch_mm_init(ptable as usize, false)
}

#[no_mangle]
pub unsafe extern "C" fn mm_defrag(mpool: *const MPool) {
    let mpool = &*mpool;
    HYPERVISOR_PAGE_TABLE.lock().defrag(mpool);
}
