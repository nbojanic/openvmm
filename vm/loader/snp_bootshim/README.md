# SNP Linux-direct runtime CPU-table prototype

This Kata fork always sizes `snp_linux_direct` guests at launch, allowing one
IGVM to boot with different CPU and RAM sizes. There is no fixed-profile mode
or `launch_time_sizing` switch.

```json
{
  "processor_count": 1,
  "memory_page_count": 131072,
  "c_bit_position": 51
}
```

These are fields inside the existing image object; the `linux` configuration is
still required. `processor_count=1` supplies only the BSP/platform template, not
the maximum launch count. The bootshim constructs actual CPU records at boot.
`memory_page_count` is the minimum RAM requirement (512 MiB in this example).
The test image uses `nr_cpus=256` to match the bootshim's bounded capacity.
The generator and bootshim must be built together: the launch parameter area
is mandatory, and legacy handoff formats are rejected.
The measured handoff uses the eight-byte magic `katashim` and starts a separate
Kata ABI at version 1. Earlier `SNPBSHIM` handoffs are rejected by their magic;
future incompatible Kata layout changes must increment the version.

## Interface and trust boundary

The generator emits the existing IGVM `VpCount`, `MemoryMap`, and `Madt`
directives into a three-page parameter area. No new VMM RPC or kernel ABI is
needed. OpenVMM imports those pages as private but unmeasured; their contents
are **untrusted launch input**, not authenticated configuration.

The measured launch-sizing `SnpBootShimParams` handoff uses `launch_params_gpa` as the
parameter-area GPA. The GPA itself remains measured. VpCount is at offset 0,
the memory map at offset 8, and host MADT at offset 4096 with an 8-KiB bound.
Four explicit padding bytes after VpCount align the memory-map entries' 64-bit
fields. The generator derives the first two offsets from the shared struct.
At most two RAM ranges followed by a zero terminator are supported.

The shim snapshots and validates the launch page, then:

- preserves the measured low-memory reservations while updating Linux E820;
- validates host MADT structure/checksum, enabled count, unique APIC IDs and
  one-based ACPI UIDs (`UID = VP index + 1`);
- constructs Local x2APIC and x2APIC-affinity records for the actual CPUs,
  preserving their APIC IDs rather than assuming IDs are consecutive;
- copies interrupt-routing/platform records only from measured templates;
- fills the measured SRAT memory-affinity records;
- reserves the 16-KiB output buffers as E820 ACPI memory and updates
  XSDT/RSDT pointers, table lengths and checksums;
- moves the measured PCI SSDT's high-MMIO window above high RAM;
- installs a private 64-GiB identity map and accepts actual omitted RAM using
  the existing GHCB/PVALIDATE path.

The host MADT supplies only CPU identities. Its APIC MMIO addresses, interrupt
routing and other platform records are ignored. A valid checksum detects
malformed data, not an authenticated host. The kernel image, entrypoint,
command line, rootfs verity parameters and PCI description remain measured.
Bad launch input terminates the guest through the existing GHCB shutdown request.

Linux still starts secondary CPUs. The old 6.1 conf-pods kernel directly
registered CPUs from VpCount; this prototype supplies normal ACPI tables to
the unchanged 6.18 kernel instead.

Changing resource counts does not change the launch measurement. Consequently
the measurement does **not** authenticate the actual CPU count or RAM size.
Resource attestation, if required, needs a separate guest-verifier protocol.
A malicious host can withhold resources or prevent boot; this interface does
not promise availability.

## Current prototype limits

- x86_64 SNP, C-bit 51, one NUMA node, at most 256 CPUs.
- APIC IDs may be non-consecutive; UIDs must cover 1 through VpCount, and the BSP
  must be APIC ID zero. Output tables are bounded to 8 KiB each, not prefilled
  with one CPU record per possible launch CPU.
- One low RAM range starting at zero, ending no later than `0xe0000000`.
  A gap below ECAM is allowed: OpenVMM can cap low RAM at 3 GiB.
- Optional high RAM starting at 4 GiB; total RAM at most 32 GiB.
- One PCI segment, ECAM at `0xe0000000`, 256-MiB low PCI window and 16-GiB high
  PCI window. The latter starts at the 1-GiB-aligned end of RAM, at least 4 GiB.
- No host-supplied AML, NUMA topology, command-line overrides, CPU hotplug or
  memory hotplug.

The PCI layout incorporates the earlier IGVM-generator PCI platform changes.
This is not a general-purpose ACPI parser or
a finalized upstream ABI. Other PCI layouts and NUMA need explicit design.
CPU power-management/hotplug namespace support is not part of this prototype.

Build and run the generator/config/bootshim tests on the remote build machine.
The bare-metal executable additionally needs a
`MINIMAL_RT_BUILD=1 cargo build -p snp_bootshim --target x86_64-unknown-none
--profile boot-release` build; host-mode unit tests do not compile that path.

## Glossary

Acronyms and related architectural terms used in this document:

| Term | Meaning in this prototype |
|---|---|
| ABI | Application Binary Interface; the binary layout and calling contract shared by the generator, bootshim and kernel. |
| ACPI | Advanced Configuration and Power Interface; firmware tables describing CPUs, memory and platform devices to Linux. |
| AML | ACPI Machine Language; executable firmware bytecode, such as device descriptions and methods in an SSDT. |
| AP | Application Processor; a secondary CPU started by Linux after the boot CPU. |
| APIC | Advanced Programmable Interrupt Controller; the x86 interrupt controller. An APIC ID identifies a CPU's interrupt destination. |
| BSP | Bootstrap Processor; the initial CPU that runs the bootshim and enters Linux. |
| C-bit | The address bit used to select encrypted/private memory in this SNP configuration; bit 51 in the current prototype. |
| CPU | Central Processing Unit; CPU counts here refer to guest virtual processors, not necessarily physical host cores. |
| E820 | The x86 firmware memory-map interface, named after BIOS interrupt `0x15`, function `0xe820`; its entries describe usable and reserved memory. |
| ECAM | Enhanced Configuration Access Mechanism; the memory-mapped PCI Express configuration space. |
| GHCB | Guest-Hypervisor Communication Block; the SEV-ES/SNP interface for communicating requests and results between the guest and hypervisor. |
| GPA | Guest Physical Address; an address in the VM's physical address space, not a host physical or virtual address. |
| IGVM | Independent Guest Virtual Machine; the image format carrying initial guest memory, processor state and launch directives. |
| KiB / MiB / GiB | Kibibyte, mebibyte and gibibyte: respectively `2^10`, `2^20` and `2^30` bytes. |
| MADT | Multiple APIC Description Table; the ACPI table containing processor interrupt-controller identities and interrupt-routing records. |
| MMIO | Memory-Mapped Input/Output; address ranges used to access devices rather than ordinary RAM. |
| NUMA | Non-Uniform Memory Access; a topology grouping CPUs and memory by locality. This prototype exposes one node. |
| PCI | Peripheral Component Interconnect; the device-bus family used for the guest platform's PCI/PCI Express devices. |
| PVALIDATE | The AMD instruction used by an SNP guest to validate or invalidate memory pages; used here when accepting additional RAM. |
| RAM | Random-Access Memory; the guest memory sized at launch. |
| rootfs | Root filesystem; supplied separately as a block-device image, with its dm-verity parameters bound into the measured command line. |
| RPC | Remote Procedure Call; the control interface used by the runtime to request VMM operations. |
| RSDT | Root System Description Table; an ACPI root table containing 32-bit addresses of other ACPI tables. |
| SEV / SEV-ES | Secure Encrypted Virtualization / SEV Encrypted State; AMD virtualization protections extended by SNP. |
| SNP | Secure Nested Paging; the AMD SEV extension providing additional guest-memory integrity and isolation protections. |
| SRAT | System Resource Affinity Table; the ACPI table associating processors and memory ranges with NUMA proximity domains. |
| SSDT | Secondary System Description Table; an ACPI table containing additional AML definitions, including this prototype's PCI resource windows. |
| UID | Unique Identifier; the ACPI processor identifier, which this OpenVMM interface sets to `VP index + 1`. It is distinct from the APIC ID. |
| VM | Virtual Machine; the guest system being launched. |
| VMM | Virtual Machine Monitor; OpenVMM, which creates and manages the guest using the underlying hypervisor. |
| VP / vCPU | Virtual Processor / virtual CPU; a guest execution context. A VP index is not necessarily its APIC ID. |
| x2APIC | Extended xAPIC; an x86 interrupt-controller interface with 32-bit APIC IDs. The bootshim emits the corresponding ACPI CPU records. |
| XSDT | Extended System Description Table; an ACPI root table containing 64-bit addresses of other ACPI tables. |
