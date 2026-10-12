// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use igvm_defs::MemoryMapEntryType;
#[cfg(minimal_rt)]
use loader_defs::linux::{
    SNP_BOOT_SHIM_CPU_TABLE_SIZE, SNP_BOOT_SHIM_MADT_OFFSET, SNP_BOOT_SHIM_MADT_SIZE,
};
use loader_defs::linux::{
    SNP_BOOT_SHIM_HIGH_MMIO_END, SNP_BOOT_SHIM_HIGH_RAM, SNP_BOOT_SHIM_LOW_MMIO,
    SNP_BOOT_SHIM_MAX_CPUS, SNP_BOOT_SHIM_MAX_GPA, SNP_BOOT_SHIM_MAX_RAM, SnpBootShimLaunchParams,
    SnpBootShimRange,
};

use super::PAGE_SIZE;

#[derive(Debug, Eq, PartialEq)]
pub enum LaunchError {
    CpuCount,
    MemoryMap,
    Acpi,
    E820,
}

pub struct LaunchMemory {
    pub ranges: [SnpBootShimRange; 2],
    pub count: usize,
    pub processor_count: u32,
}

pub fn validate_launch(
    input: &SnpBootShimLaunchParams,
    minimum_ram: u64,
) -> Result<LaunchMemory, LaunchError> {
    if input.processor_count == 0 || input.processor_count as usize > SNP_BOOT_SHIM_MAX_CPUS {
        return Err(LaunchError::CpuCount);
    }
    if input.reserved != [0; 4] {
        return Err(LaunchError::MemoryMap);
    }
    let mut memory = LaunchMemory {
        ranges: [SnpBootShimRange {
            start_gpn: 0,
            page_count: 0,
        }; 2],
        count: 0,
        processor_count: input.processor_count,
    };
    let mut total = 0u64;
    let mut terminated = false;
    for entry in &input.memory_map {
        if entry.entry_type != MemoryMapEntryType::MEMORY || entry.flags != 0 || entry.reserved != 0
        {
            return Err(LaunchError::MemoryMap);
        }
        if entry.number_of_pages == 0 {
            if entry.starting_gpa_page_number != 0 {
                return Err(LaunchError::MemoryMap);
            }
            terminated = true;
            continue;
        }
        if terminated || memory.count == memory.ranges.len() {
            return Err(LaunchError::MemoryMap);
        }
        let start = entry
            .starting_gpa_page_number
            .checked_mul(PAGE_SIZE)
            .ok_or(LaunchError::MemoryMap)?;
        let size = entry
            .number_of_pages
            .checked_mul(PAGE_SIZE)
            .ok_or(LaunchError::MemoryMap)?;
        let end = start.checked_add(size).ok_or(LaunchError::MemoryMap)?;
        if end > SNP_BOOT_SHIM_MAX_GPA {
            return Err(LaunchError::MemoryMap);
        }
        if memory.count == 0 {
            if start != 0 || end < minimum_ram || end > SNP_BOOT_SHIM_LOW_MMIO {
                return Err(LaunchError::MemoryMap);
            }
        } else if start != SNP_BOOT_SHIM_HIGH_RAM {
            return Err(LaunchError::MemoryMap);
        }
        total = total.checked_add(size).ok_or(LaunchError::MemoryMap)?;
        memory.ranges[memory.count] = SnpBootShimRange {
            start_gpn: entry.starting_gpa_page_number,
            page_count: entry.number_of_pages,
        };
        memory.count += 1;
    }
    if !terminated || memory.count == 0 || total > SNP_BOOT_SHIM_MAX_RAM {
        return Err(LaunchError::MemoryMap);
    }
    Ok(memory)
}

pub(super) fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, LaunchError> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or(LaunchError::Acpi)?
            .try_into()
            .unwrap(),
    ))
}

pub(super) fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, LaunchError> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or(LaunchError::Acpi)?
            .try_into()
            .unwrap(),
    ))
}

pub(super) fn fix_checksum(table: &mut [u8]) {
    table[9] = 0;
    table[9] = 0u8.wrapping_sub(table.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)));
}

fn update_pci_window(table: &mut [u8], memory: &LaunchMemory) -> Result<(), LaunchError> {
    let mut matches = 0;
    // Match the complete high-MMIO QwordMemory descriptor in our measured
    // PCI SSDT, not arbitrary host-provided AML.
    for offset in 36..table.len().saturating_sub(45) {
        if table[offset..offset + 5] != [0x8a, 0x2b, 0, 0, 0xc] {
            continue;
        }
        if read_u64(table, offset + 14)? != SNP_BOOT_SHIM_HIGH_RAM
            || read_u64(table, offset + 22)? != SNP_BOOT_SHIM_HIGH_MMIO_END - 1
            || read_u64(table, offset + 38)? != SNP_BOOT_SHIM_HIGH_MMIO_END - SNP_BOOT_SHIM_HIGH_RAM
        {
            continue;
        }
        let last = memory.ranges[memory.count - 1];
        let ram_end = (last.start_gpn + last.page_count) * PAGE_SIZE;
        let base = ram_end
            .max(SNP_BOOT_SHIM_HIGH_RAM)
            .next_multiple_of(1 << 30);
        let length = SNP_BOOT_SHIM_HIGH_MMIO_END - SNP_BOOT_SHIM_HIGH_RAM;
        write_u64(table, offset + 14, base)?;
        write_u64(table, offset + 22, base + length - 1)?;
        matches += 1;
    }
    if matches != 1 {
        return Err(LaunchError::Acpi);
    }
    fix_checksum(table);
    Ok(())
}

pub(super) fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), LaunchError> {
    bytes
        .get_mut(offset..offset + 4)
        .ok_or(LaunchError::Acpi)?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

pub(super) fn write_u64(bytes: &mut [u8], offset: usize, value: u64) -> Result<(), LaunchError> {
    bytes
        .get_mut(offset..offset + 8)
        .ok_or(LaunchError::Acpi)?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[cfg(minimal_rt)]
#[repr(C, align(4096))]
struct CpuTables {
    madt: [u8; SNP_BOOT_SHIM_CPU_TABLE_SIZE],
    srat: [u8; SNP_BOOT_SHIM_CPU_TABLE_SIZE],
}

#[cfg(minimal_rt)]
static mut CPU_TABLES: CpuTables = CpuTables {
    madt: [0; SNP_BOOT_SHIM_CPU_TABLE_SIZE],
    srat: [0; SNP_BOOT_SHIM_CPU_TABLE_SIZE],
};

#[cfg(minimal_rt)]
static mut HOST_MADT: [u8; SNP_BOOT_SHIM_MADT_SIZE] = [0; SNP_BOOT_SHIM_MADT_SIZE];

#[cfg(minimal_rt)]
pub fn update_linux(
    params: &loader_defs::linux::SnpBootShimParams,
    memory: &LaunchMemory,
) -> Result<(), LaunchError> {
    use loader_defs::linux::{E820_RAM, boot_params, e820entry};

    let snapshot_ptr = &raw mut HOST_MADT;
    // SAFETY: The measured handoff bounds the imported parameter area. This
    // single-threaded snapshot does not accept any host pointer from its data.
    let snapshot = unsafe {
        core::ptr::copy_nonoverlapping(
            (params.launch_params_gpa + u64::from(SNP_BOOT_SHIM_MADT_OFFSET)) as *const u8,
            snapshot_ptr.cast::<u8>(),
            SNP_BOOT_SHIM_MADT_SIZE,
        );
        &*snapshot_ptr
    };
    let topology = crate::cpu_tables::parse_host_madt(snapshot, memory.processor_count)?;
    let tables_ptr = &raw mut CPU_TABLES;
    // SAFETY: The BSP exclusively owns these measured, imported output pages.
    let tables = unsafe { &mut *tables_ptr };
    let table_base = tables_ptr as u64;
    let table_size = size_of::<CpuTables>() as u64;
    if table_base + table_size > params.ram_end {
        return Err(LaunchError::E820);
    }
    // SAFETY: The measured handoff identifies this already-validated page.
    let zero = unsafe { &mut *(params.linux_zero_page as *mut boot_params) };
    let mut extended = false;
    let count = zero.e820_entries as usize;
    for entry in zero.e820_map.get_mut(..count).ok_or(LaunchError::E820)? {
        let start = u64::from(entry.addr);
        if u32::from(entry.typ) == E820_RAM
            && start >= 0x10_0000
            && start.checked_add(u64::from(entry.size)) == Some(params.ram_end)
        {
            entry.size = (memory.ranges[0].page_count * PAGE_SIZE - start).into();
            extended = true;
        }
    }
    if !extended {
        return Err(LaunchError::E820);
    }
    if memory.count == 2 {
        let range = memory.ranges[1];
        *zero.e820_map.get_mut(count).ok_or(LaunchError::E820)? = e820entry {
            addr: (range.start_gpn * PAGE_SIZE).into(),
            size: (range.page_count * PAGE_SIZE).into(),
            typ: E820_RAM.into(),
        };
        zero.e820_entries += 1;
    }
    crate::cpu_tables::reserve_tables(zero, table_base, table_size)?;

    // SAFETY: The Linux loader imports the RSDP at this fixed measured GPA.
    let rsdp = unsafe { core::slice::from_raw_parts(0xe0000 as *const u8, 36) };
    if &rsdp[..8] != b"RSD PTR " {
        return Err(LaunchError::Acpi);
    }
    let xsdt_gpa = u64::from_le_bytes(rsdp[24..32].try_into().unwrap());
    let rsdt_gpa = u64::from(read_u32(rsdp, 16)?);
    let mut addresses = [0u64; 32];
    let count = {
        let xsdt = measured_table(xsdt_gpa)?;
        if &xsdt[..4] != b"XSDT" || (xsdt.len() - 36) % 8 != 0 {
            return Err(LaunchError::Acpi);
        }
        let count = (xsdt.len() - 36) / 8;
        if count > addresses.len() {
            return Err(LaunchError::Acpi);
        }
        for (address, bytes) in addresses.iter_mut().zip(xsdt[36..].as_chunks::<8>().0) {
            *address = u64::from_le_bytes(*bytes);
        }
        count
    };
    let mut found_madt = false;
    let mut found_srat = false;
    let mut found_pci = false;
    let mut replacements = [(0u64, 0u64); 2];
    for &address in &addresses[..count] {
        let table = measured_table(address)?;
        match &table[..4] {
            b"APIC" if !found_madt => {
                crate::cpu_tables::build_madt(table, &topology, &mut tables.madt)?;
                replacements[0] = (address, tables.madt.as_ptr() as u64);
                found_madt = true;
            }
            b"SRAT" if !found_srat => {
                crate::cpu_tables::build_srat(table, &topology, memory, &mut tables.srat)?;
                replacements[1] = (address, tables.srat.as_ptr() as u64);
                found_srat = true;
            }
            b"SSDT" if !found_pci => {
                update_pci_window(table, memory)?;
                found_pci = true;
            }
            b"APIC" | b"SRAT" | b"SSDT" => return Err(LaunchError::Acpi),
            _ => {}
        }
    }
    if !found_madt || !found_srat || !found_pci {
        return Err(LaunchError::Acpi);
    }
    crate::cpu_tables::repoint_root(measured_table(xsdt_gpa)?, &replacements)?;
    if rsdt_gpa != 0 {
        crate::cpu_tables::repoint_root(measured_table(rsdt_gpa)?, &replacements)?;
    }
    Ok(())
}

#[cfg(minimal_rt)]
fn measured_table(address: u64) -> Result<&'static mut [u8], LaunchError> {
    if !(0x1000..0xe0000 - 36).contains(&address) {
        return Err(LaunchError::Acpi);
    }
    // SAFETY: Only measured RSDP/XSDT pointers are passed here. Bounds exclude
    // the Linux image and launch-parameter page; table accesses are sequential.
    let header = unsafe { core::slice::from_raw_parts(address as *const u8, 36) };
    let length = read_u32(header, 4)? as usize;
    if length < 36 || address + length as u64 > 0xe0000 {
        return Err(LaunchError::Acpi);
    }
    // SAFETY: The measured table's complete extent was checked above.
    Ok(unsafe { core::slice::from_raw_parts_mut(address as *mut u8, length) })
}

#[cfg(minimal_rt)]
#[repr(C, align(4096))]
#[derive(Copy, Clone)]
struct PageTable([u64; 512]);

#[cfg(minimal_rt)]
static mut PAGE_TABLES: [PageTable; 66] = [PageTable([0; 512]); 66];

#[cfg(minimal_rt)]
pub fn map_high_memory() {
    const C_BIT: u64 = 1 << 51;
    let table_ptr = &raw mut PAGE_TABLES;
    // SAFETY: Single-threaded bootstrap owns these measured BSS pages. They
    // reside below the minimum RAM limit and are mapped by the initial CR3.
    let tables = unsafe { &mut *table_ptr };
    tables[0].0[0] = (&raw const tables[1]) as u64 | C_BIT | 3;
    for gb in 0..64 {
        tables[1].0[gb] = (&raw const tables[gb + 2]) as u64 | C_BIT | 3;
        for page in 0..512 {
            tables[gb + 2].0[page] = ((gb as u64) << 30) | ((page as u64) << 21) | C_BIT | 0x83;
        }
    }
    let cr3 = (&raw const tables[0]) as u64 | C_BIT;
    // SAFETY: These tables identity-map all bootstrap code/data and the
    // bounded guest RAM address space with private 2-MiB entries.
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use igvm_defs::IGVM_VHS_MEMORY_MAP_ENTRY;

    fn input() -> SnpBootShimLaunchParams {
        let empty = IGVM_VHS_MEMORY_MAP_ENTRY {
            starting_gpa_page_number: 0,
            number_of_pages: 0,
            entry_type: MemoryMapEntryType::MEMORY,
            flags: 0,
            reserved: 0,
        };
        let mut p = SnpBootShimLaunchParams {
            processor_count: 2,
            reserved: [0; 4],
            memory_map: [empty; 3],
        };
        p.memory_map[0].number_of_pages = (1 << 30) / PAGE_SIZE;
        p
    }

    #[test]
    fn validates_low_and_high_ram() {
        let mut p = input();
        assert_eq!(validate_launch(&p, 512 << 20).unwrap().count, 1);
        p.memory_map[0].number_of_pages = SNP_BOOT_SHIM_LOW_MMIO / PAGE_SIZE;
        p.memory_map[1].starting_gpa_page_number = SNP_BOOT_SHIM_HIGH_RAM / PAGE_SIZE;
        p.memory_map[1].number_of_pages = (2 << 30) / PAGE_SIZE;
        assert_eq!(validate_launch(&p, 512 << 20).unwrap().count, 2);
        p.memory_map[0].number_of_pages = (3 << 30) / PAGE_SIZE;
        assert_eq!(validate_launch(&p, 512 << 20).unwrap().count, 2);
    }

    #[test]
    fn rejects_bad_launch_inputs() {
        let mut p = input();
        p.processor_count = 0;
        assert!(validate_launch(&p, 512 << 20).is_err());
        p = input();
        p.memory_map[0].number_of_pages = 1;
        assert!(validate_launch(&p, 512 << 20).is_err());
        p = input();
        p.memory_map[0].number_of_pages = u64::MAX;
        assert!(validate_launch(&p, 512 << 20).is_err());
        p = input();
        p.memory_map[0].entry_type = MemoryMapEntryType::VTL2_PROTECTABLE;
        assert!(validate_launch(&p, 512 << 20).is_err());
        p = input();
        p.memory_map[0].number_of_pages = SNP_BOOT_SHIM_LOW_MMIO / PAGE_SIZE;
        p.memory_map[1].starting_gpa_page_number = SNP_BOOT_SHIM_LOW_MMIO / PAGE_SIZE;
        p.memory_map[1].number_of_pages = 1;
        assert!(validate_launch(&p, 512 << 20).is_err());
    }

    #[test]
    fn pci_mmio_moves_above_high_ram() {
        let mut p = input();
        p.memory_map[0].number_of_pages = SNP_BOOT_SHIM_LOW_MMIO / PAGE_SIZE;
        p.memory_map[1].starting_gpa_page_number = SNP_BOOT_SHIM_HIGH_RAM / PAGE_SIZE;
        p.memory_map[1].number_of_pages = (1536 << 20) / PAGE_SIZE;
        let memory = validate_launch(&p, 512 << 20).unwrap();
        let mut table = vec![0u8; 36 + 46];
        table[..4].copy_from_slice(b"SSDT");
        table[36..41].copy_from_slice(&[0x8a, 0x2b, 0, 0, 0xc]);
        write_u64(&mut table, 50, SNP_BOOT_SHIM_HIGH_RAM).unwrap();
        write_u64(&mut table, 58, SNP_BOOT_SHIM_HIGH_MMIO_END - 1).unwrap();
        write_u64(
            &mut table,
            74,
            SNP_BOOT_SHIM_HIGH_MMIO_END - SNP_BOOT_SHIM_HIGH_RAM,
        )
        .unwrap();
        update_pci_window(&mut table, &memory).unwrap();
        assert_eq!(read_u64(&table, 50).unwrap(), 6 << 30);
        assert_eq!(read_u64(&table, 58).unwrap(), (22 << 30) - 1);
        assert_eq!(table.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
    }
}
