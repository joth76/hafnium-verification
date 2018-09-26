#pragma once

#include <stdbool.h>
#include <stddef.h>

#include "hf/addr.h"

/* A page table entry. */
typedef uint64_t pte_t;

#define PAGE_LEVEL_BITS 9

#define ARCH_AARCH64_MM_PTE_ATTR_MASK \
	(((UINT64_C(1) << PAGE_BITS) - 1) | ~((UINT64_C(1) << 48) - 1))

/**
 * Returns the encoding of a page table entry that isn't present.
 */
static inline pte_t arch_mm_absent_pte(int level)
{
	return 0;
}

/**
 * Converts a physical address to a table PTE.
 *
 * The spec says that 'Table descriptors for stage 2 translations do not
 * include any attribute field', so we don't take any attributes as arguments.
 */
static inline pte_t arch_mm_table_pte(int level, paddr_t pa)
{
	/* This is the same for all levels on aarch64. */
	(void)level;
	return pa_addr(pa) | 0x3;
}

/**
 * Converts a physical address to a block PTE.
 *
 * The level must allow block entries.
 */
static inline pte_t arch_mm_block_pte(int level, paddr_t pa, uint64_t attrs)
{
	pte_t pte = pa_addr(pa) | attrs;
	if (level == 0) {
		/* A level 0 'block' is actually a page entry. */
		pte |= 0x2;
	}
	return pte;
}

/**
 * Specifies whether block mappings are acceptable at the given level.
 *
 * Level 0 must allow block entries.
 */
static inline bool arch_mm_is_block_allowed(int level)
{
	return level <= 2;
}

/**
 * Determines if the given pte is present, i.e., if it points to another table,
 * to a page, or a block of pages.
 */
static inline bool arch_mm_pte_is_present(pte_t pte, int level)
{
	return (pte & 0x1) != 0;
}

/**
 * Determines if the given pte references another table.
 */
static inline bool arch_mm_pte_is_table(pte_t pte, int level)
{
	return level != 0 && (pte & 0x3) == 0x3;
}

/**
 * Determines if the given pte references a block of pages.
 */
static inline bool arch_mm_pte_is_block(pte_t pte, int level)
{
	/* We count pages at level 0 as blocks. */
	return arch_mm_is_block_allowed(level) &&
	       (pte & 0x3) == (level == 0 ? 0x3 : 0x1);
}

static inline uint64_t arch_aarch64_mm_clear_pte_attrs(pte_t pte)
{
	return pte & ~ARCH_AARCH64_MM_PTE_ATTR_MASK;
}

/**
 * Clears the given physical address, i.e., sets the ignored bits (from a page
 * table perspective) to zero.
 */
static inline paddr_t arch_mm_clear_pa(paddr_t pa)
{
	return pa_init(arch_aarch64_mm_clear_pte_attrs(pa_addr(pa)));
}

/**
 * Extracts the physical address of the block referred to by the given page
 * table entry.
 */
static inline paddr_t arch_mm_block_from_pte(pte_t pte)
{
	return pa_init(arch_aarch64_mm_clear_pte_attrs(pte));
}

/**
 * Extracts the physical address of the page table referred to by the given page
 * table entry.
 */
static inline paddr_t arch_mm_table_from_pte(pte_t pte)
{
	return pa_init(arch_aarch64_mm_clear_pte_attrs(pte));
}

/**
 * Extracts the architecture specific attributes applies to the given page table
 * entry.
 */
static inline uint64_t arch_mm_pte_attrs(pte_t pte)
{
	return pte & ARCH_AARCH64_MM_PTE_ATTR_MASK;
}

/**
 * Invalidates stage-1 TLB entries referring to the given virtual address range.
 */
static inline void arch_mm_invalidate_stage1_range(vaddr_t va_begin,
						   vaddr_t va_end)
{
	uintvaddr_t begin = va_addr(va_begin);
	uintvaddr_t end = va_addr(va_end);
	uintvaddr_t it;

	begin >>= 12;
	end >>= 12;

	__asm__ volatile("dsb ishst");

	for (it = begin; it < end; it += (UINT64_C(1) << (PAGE_BITS - 12))) {
		__asm__("tlbi vae2is, %0" : : "r"(it));
	}

	__asm__ volatile("dsb ish");
}

/**
 * Invalidates stage-2 TLB entries referring to the given intermediate physical
 * address range.
 */
static inline void arch_mm_invalidate_stage2_range(ipaddr_t va_begin,
						   ipaddr_t va_end)
{
	uintpaddr_t begin = ipa_addr(va_begin);
	uintpaddr_t end = ipa_addr(va_end);
	uintpaddr_t it;

	/* TODO: This only applies to the current VMID. */

	begin >>= 12;
	end >>= 12;

	__asm__ volatile("dsb ishst");

	for (it = begin; it < end; it += (UINT64_C(1) << (PAGE_BITS - 12))) {
		__asm__("tlbi ipas2e1, %0" : : "r"(it));
	}

	__asm__ volatile(
		"dsb ish\n"
		"tlbi vmalle1is\n"
		"dsb ish\n");
}

static inline void arch_mm_set_vm(uint64_t vmid, paddr_t table)
{
	__asm__ volatile("msr vttbr_el2, %0"
			 :
			 : "r"(pa_addr(table) | (vmid << 48)));
}

uint64_t arch_mm_mode_to_attrs(int mode);
bool arch_mm_init(paddr_t table, bool first);
int arch_mm_max_level(int mode);
