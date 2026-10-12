// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build CPU records from validated launch identities, retaining only measured
//! platform records. Host ACPI addresses, interrupt routing and AML are not used.

use crate::launch::{
    LaunchError, LaunchMemory, fix_checksum, read_u32, read_u64, write_u32, write_u64,
};
use loader_defs::linux::{SNP_BOOT_SHIM_MADT_SIZE, SNP_BOOT_SHIM_MAX_CPUS};

pub struct CpuTopology {
    apic_ids: [u32; SNP_BOOT_SHIM_MAX_CPUS],
    count: usize,
}

fn table<'a>(data: &'a [u8], signature: &[u8; 4], minimum: usize) -> Result<&'a [u8], LaunchError> {
    if data.get(..4) != Some(signature) {
        return Err(LaunchError::Acpi);
    }
    let length = read_u32(data, 4)? as usize;
    if length < minimum {
        return Err(LaunchError::Acpi);
    }
    let data = data.get(..length).ok_or(LaunchError::Acpi)?;
    if data.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)) != 0 {
        return Err(LaunchError::Acpi);
    }
    Ok(data)
}

fn record(data: &[u8], offset: usize) -> Result<&[u8], LaunchError> {
    let length = *data.get(offset + 1).ok_or(LaunchError::Acpi)? as usize;
    if length < 2 {
        return Err(LaunchError::Acpi);
    }
    data.get(offset..offset + length).ok_or(LaunchError::Acpi)
}

pub fn parse_host_madt(data: &[u8], processor_count: u32) -> Result<CpuTopology, LaunchError> {
    if processor_count == 0 || processor_count as usize > SNP_BOOT_SHIM_MAX_CPUS {
        return Err(LaunchError::CpuCount);
    }
    let data = table(data, b"APIC", 44)?;
    if data.len() > SNP_BOOT_SHIM_MADT_SIZE {
        return Err(LaunchError::Acpi);
    }
    let mut topology = CpuTopology {
        apic_ids: [u32::MAX; SNP_BOOT_SHIM_MAX_CPUS],
        count: processor_count as usize,
    };
    let mut offset = 44;
    let mut enabled = 0;
    while offset < data.len() {
        let entry = record(data, offset)?;
        let cpu = match entry[0] {
            0 if entry.len() == 8 => Some((entry[2] as u32, entry[3] as u32, read_u32(entry, 4)?)),
            9 if entry.len() == 16 && entry[2..4] == [0, 0] => Some((
                read_u32(entry, 12)?,
                read_u32(entry, 4)?,
                read_u32(entry, 8)?,
            )),
            0 | 9 => return Err(LaunchError::Acpi),
            _ => None,
        };
        if let Some((uid, apic_id, flags)) = cpu {
            if flags & !3 != 0 {
                return Err(LaunchError::Acpi);
            }
            if flags & 1 != 0 {
                // OpenVMM assigns ACPI UID = VP index + 1. Keep that ordering
                // while allowing non-consecutive physical APIC IDs.
                if uid == 0
                    || uid > processor_count
                    || apic_id == u32::MAX
                    || topology.apic_ids[uid as usize - 1] != u32::MAX
                    || topology.apic_ids[..topology.count].contains(&apic_id)
                {
                    return Err(LaunchError::Acpi);
                }
                topology.apic_ids[uid as usize - 1] = apic_id;
                enabled += 1;
            }
        }
        offset += entry.len();
    }
    if enabled != topology.count || topology.apic_ids[0] != 0 {
        return Err(LaunchError::CpuCount);
    }
    Ok(topology)
}

fn append(out: &mut [u8], used: &mut usize, bytes: &[u8]) -> Result<(), LaunchError> {
    let end = used.checked_add(bytes.len()).ok_or(LaunchError::Acpi)?;
    out.get_mut(*used..end)
        .ok_or(LaunchError::Acpi)?
        .copy_from_slice(bytes);
    *used = end;
    Ok(())
}

fn finish(out: &mut [u8], used: usize) -> Result<usize, LaunchError> {
    write_u32(out, 4, used.try_into().map_err(|_| LaunchError::Acpi)?)?;
    fix_checksum(&mut out[..used]);
    Ok(used)
}

pub fn build_madt(
    measured: &[u8],
    topology: &CpuTopology,
    out: &mut [u8],
) -> Result<usize, LaunchError> {
    let measured = table(measured, b"APIC", 44)?;
    let mut used = 0;
    append(out, &mut used, &measured[..44])?;
    let mut offset = 44;
    while offset < measured.len() {
        let entry = record(measured, offset)?;
        if !matches!(entry[0], 0 | 9) {
            append(out, &mut used, entry)?;
        }
        offset += entry.len();
    }
    for (uid, &apic_id) in topology.apic_ids[..topology.count].iter().enumerate() {
        let mut entry = [0u8; 16];
        entry[0] = 9; // Processor Local x2APIC
        entry[1] = 16;
        write_u32(&mut entry, 4, apic_id)?;
        write_u32(&mut entry, 8, 1)?;
        write_u32(&mut entry, 12, uid as u32 + 1)?;
        append(out, &mut used, &entry)?;
    }
    finish(out, used)
}

pub fn build_srat(
    measured: &[u8],
    topology: &CpuTopology,
    memory: &LaunchMemory,
    out: &mut [u8],
) -> Result<usize, LaunchError> {
    if topology.count != memory.processor_count as usize {
        return Err(LaunchError::CpuCount);
    }
    let measured = table(measured, b"SRAT", 48)?;
    let mut used = 0;
    append(out, &mut used, &measured[..48])?;
    let mut offset = 48;
    let mut slots = 0;
    while offset < measured.len() {
        let entry = record(measured, offset)?;
        if !matches!(entry[0], 0 | 2) {
            let entry_start = used;
            append(out, &mut used, entry)?;
            if entry[0] == 1 {
                if entry.len() != 40 || slots >= memory.ranges.len() || read_u32(entry, 2)? != 0 {
                    return Err(LaunchError::Acpi);
                }
                let range = memory.ranges[slots];
                let entry = &mut out[entry_start..used];
                write_u64(entry, 8, range.start_gpn * 4096)?;
                write_u64(entry, 16, range.page_count * 4096)?;
                write_u32(entry, 28, u32::from(slots < memory.count))?;
                slots += 1;
            }
        }
        offset += entry.len();
    }
    if slots != 2 {
        return Err(LaunchError::Acpi);
    }
    for &apic_id in &topology.apic_ids[..topology.count] {
        let mut entry = [0u8; 24];
        entry[0] = 2; // Processor Local x2APIC Affinity, NUMA node zero
        entry[1] = 24;
        write_u32(&mut entry, 8, apic_id)?;
        write_u32(&mut entry, 12, 1)?;
        append(out, &mut used, &entry)?;
    }
    finish(out, used)
}

pub fn repoint_root(root: &mut [u8], replacements: &[(u64, u64); 2]) -> Result<(), LaunchError> {
    let width = match root.get(..4) {
        Some(b"XSDT") => 8,
        Some(b"RSDT") => 4,
        _ => return Err(LaunchError::Acpi),
    };
    if root.len() < 36 || !(root.len() - 36).is_multiple_of(width) {
        return Err(LaunchError::Acpi);
    }
    let mut updated = 0;
    for offset in (36..root.len()).step_by(width) {
        let address = if width == 8 {
            read_u64(root, offset)?
        } else {
            u64::from(read_u32(root, offset)?)
        };
        if let Some(&(_, replacement)) = replacements.iter().find(|&&(old, _)| old == address) {
            if width == 8 {
                write_u64(root, offset, replacement)?;
            } else {
                write_u32(
                    root,
                    offset,
                    replacement.try_into().map_err(|_| LaunchError::Acpi)?,
                )?;
            }
            updated += 1;
        }
    }
    if updated != replacements.len() {
        return Err(LaunchError::Acpi);
    }
    fix_checksum(root);
    Ok(())
}

pub fn reserve_tables(
    zero: &mut loader_defs::linux::boot_params,
    start: u64,
    size: u64,
) -> Result<(), LaunchError> {
    use loader_defs::linux::{E820_ACPI, E820_RAM, e820entry};
    let end = start.checked_add(size).ok_or(LaunchError::E820)?;
    let count = zero.e820_entries as usize;
    if size == 0
        || !start.is_multiple_of(4096)
        || !size.is_multiple_of(4096)
        || count + 2 > zero.e820_map.len()
    {
        return Err(LaunchError::E820);
    }
    for index in 0..count {
        let entry = zero.e820_map[index];
        let ram_start = u64::from(entry.addr);
        let ram_end = ram_start
            .checked_add(u64::from(entry.size))
            .ok_or(LaunchError::E820)?;
        if u32::from(entry.typ) == E820_RAM && start > ram_start && end < ram_end {
            zero.e820_map.copy_within(index + 1..count, index + 3);
            zero.e820_map[index].size = (start - ram_start).into();
            zero.e820_map[index + 1] = e820entry {
                addr: start.into(),
                size: size.into(),
                typ: E820_ACPI.into(),
            };
            zero.e820_map[index + 2] = e820entry {
                addr: end.into(),
                size: (ram_end - end).into(),
                typ: E820_RAM.into(),
            };
            zero.e820_entries += 2;
            return Ok(());
        }
    }
    Err(LaunchError::E820)
}

#[cfg(test)]
mod tests {
    use super::*;
    use loader_defs::linux::{SNP_BOOT_SHIM_CPU_TABLE_SIZE, SnpBootShimRange};

    fn host_madt(ids: &[u32]) -> Vec<u8> {
        let mut table = vec![0u8; 44 + 16 * ids.len()];
        table[..4].copy_from_slice(b"APIC");
        for (uid, id) in ids.iter().enumerate() {
            let offset = 44 + uid * 16;
            table[offset] = 9;
            table[offset + 1] = 16;
            write_u32(&mut table, offset + 4, *id).unwrap();
            write_u32(&mut table, offset + 8, 1).unwrap();
            write_u32(&mut table, offset + 12, uid as u32 + 1).unwrap();
        }
        let length = table.len();
        finish(&mut table, length).unwrap();
        table
    }

    #[test]
    fn builds_runtime_cpu_count_and_preserves_apic_gaps() {
        let host = host_madt(&[0, 2, 64, 512]);
        let topology = parse_host_madt(&host, 4).unwrap();
        let measured = host_madt(&[0]);
        let mut out = [0u8; SNP_BOOT_SHIM_CPU_TABLE_SIZE];
        let length = build_madt(&measured, &topology, &mut out).unwrap();
        let result = parse_host_madt(&out[..length], 4).unwrap();
        assert_eq!(&result.apic_ids[..4], &[0, 2, 64, 512]);
    }

    #[test]
    fn rejects_bad_host_topology() {
        assert!(parse_host_madt(&host_madt(&[0]), 0).is_err());
        assert!(parse_host_madt(&host_madt(&[0]), 257).is_err());
        assert!(parse_host_madt(&host_madt(&[0, 0]), 2).is_err());
        assert!(parse_host_madt(&host_madt(&[0, 2]), 3).is_err());
        assert!(parse_host_madt(&host_madt(&[1, 2]), 2).is_err());
        assert!(parse_host_madt(&host_madt(&[0, u32::MAX]), 2).is_err());
        let mut bad = host_madt(&[0, 2]);
        bad[9] ^= 1;
        assert!(parse_host_madt(&bad, 2).is_err());
        assert!(parse_host_madt(&host_madt(&[0])[..43], 1).is_err());
        let mut bad_uid = host_madt(&[0, 2]);
        write_u32(&mut bad_uid, 44 + 12, 0).unwrap();
        let length = bad_uid.len();
        finish(&mut bad_uid, length).unwrap();
        assert!(parse_host_madt(&bad_uid, 2).is_err());
        write_u32(&mut bad_uid, 44 + 12, 2).unwrap();
        finish(&mut bad_uid, length).unwrap();
        assert!(parse_host_madt(&bad_uid, 2).is_err());
    }

    #[test]
    fn ignores_host_interrupt_routing_and_keeps_measured_records() {
        let mut measured = host_madt(&[0]);
        let ioapic = [1, 12, 7, 0, 0, 0, 0xc0, 0xfe, 0, 0, 0, 0];
        measured.extend(ioapic);
        let length = measured.len();
        finish(&mut measured, length).unwrap();
        let mut host = host_madt(&[0, 16]);
        host.extend([1, 12, 99, 0, 1, 2, 3, 4, 5, 6, 7, 8]);
        let length = host.len();
        finish(&mut host, length).unwrap();
        let topology = parse_host_madt(&host, 2).unwrap();
        let mut out = [0u8; SNP_BOOT_SHIM_CPU_TABLE_SIZE];
        build_madt(&measured, &topology, &mut out).unwrap();
        assert_eq!(&out[44..56], &ioapic);
    }

    #[test]
    fn maximum_topology_fits_bounded_tables() {
        let ids = (0..SNP_BOOT_SHIM_MAX_CPUS as u32).collect::<Vec<_>>();
        let topology = parse_host_madt(&host_madt(&ids), ids.len() as u32).unwrap();
        let mut out = [0u8; SNP_BOOT_SHIM_CPU_TABLE_SIZE];
        assert!(build_madt(&host_madt(&[0]), &topology, &mut out).is_ok());
        assert!(build_madt(&host_madt(&[0]), &topology, &mut out[..128]).is_err());
        let mut measured = vec![0u8; 48 + 2 * 40];
        measured[..4].copy_from_slice(b"SRAT");
        for offset in [48, 88] {
            measured[offset] = 1;
            measured[offset + 1] = 40;
        }
        let length = measured.len();
        finish(&mut measured, length).unwrap();
        let memory = LaunchMemory {
            ranges: [
                SnpBootShimRange {
                    start_gpn: 0,
                    page_count: (3 << 30) / 4096,
                },
                SnpBootShimRange {
                    start_gpn: (4 << 30) / 4096,
                    page_count: (5 << 30) / 4096,
                },
            ],
            count: 2,
            processor_count: ids.len() as u32,
        };
        let length = build_srat(&measured, &topology, &memory, &mut out).unwrap();
        assert_eq!(length, 128 + ids.len() * 24);
        assert_eq!(read_u64(&out, 48 + 16).unwrap(), 3 << 30);
        assert_eq!(read_u32(&out, 128 + 8).unwrap(), 0);
        assert_eq!(read_u32(&out, length - 24 + 8).unwrap(), 255);
        assert_eq!(out[..length].iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
    }

    #[test]
    fn reserves_output_tables_without_changing_ram_total() {
        use loader_defs::linux::{E820_ACPI, E820_RAM, boot_params, e820entry};
        let mut zero = boot_params {
            e820_entries: 1,
            ..Default::default()
        };
        zero.e820_map[0] = e820entry {
            addr: 0u64.into(),
            size: (1u64 << 30).into(),
            typ: E820_RAM.into(),
        };
        reserve_tables(&mut zero, 64 << 20, 16384).unwrap();
        assert_eq!(zero.e820_entries, 3);
        assert_eq!(u32::from(zero.e820_map[1].typ), E820_ACPI);
        assert_eq!(
            zero.e820_map[..3]
                .iter()
                .map(|e| u64::from(e.size))
                .sum::<u64>(),
            1 << 30
        );
        assert!(reserve_tables(&mut zero, 64 << 20, 16384).is_err());
    }

    #[test]
    fn updates_root_table_pointers_and_checksum() {
        let mut root = [0u8; 52];
        root[..4].copy_from_slice(b"XSDT");
        write_u64(&mut root, 36, 0x19000).unwrap();
        write_u64(&mut root, 44, 0x19100).unwrap();
        repoint_root(&mut root, &[(0x19000, 0x4000000), (0x19100, 0x4002000)]).unwrap();
        assert_eq!(read_u64(&root, 36).unwrap(), 0x4000000);
        assert_eq!(root.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
    }
}
