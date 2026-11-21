//! The machine the kernel runs on. Exactly one platform is compiled in, chosen by a cargo feature; it provides:
//!
//! * the early devices as types for the traits in `hal.rs`: `Uart` (Console), `Intc` (IrqController), `Power` (PowerControl), `Clock` (Rtc);
//! * constants: `RAM_BASE`, `LINK_BASE`, `RAM_UNMAPPED_BELOW`, `HAS_SMMU`, `SWAP_*`, `CONSOLE_HEAP_PAGES`, `BANNER_SUFFIX`, `SEED_SOURCE`;
//! * the machine's description (QEMU: read from the device tree firmware passes in x0): `set_dtb`, `init_machine`, `cpu_count`, `cpu_mpidr`, `core_is_little`, `intc_name`;
//! * hooks: `map_devices`, `psci_power`, `psci_cpu_on`, `psci_core_exists`, `exit_machine`, `early_boot`, `entropy_report`, `init_devices`, `irq`, `probe_devices`,
//!   `boot_caps`, `spawn_extra_tasks`, `rtc_seconds`, `device_secret`, `gpu_name`, `storage_name`, `iommu_note` (what `fetch` calls the display and the storage, and why there is no IOMMU);
//! * the boot-time parameters and test switches: `ram_size`, `ram_hole` (a range inside the RAM span that is not RAM: another bank's gap, a device window; `None` after the last), `flag`, `number`, `bytes`, `init_flags`, `interactive`, `map_log`, `only`, `fuzz_seed`,
//!   `chaos_seed`.
//!
//! The shared kernel files call only these names, never a machine's addresses. To add a machine: write `platform/<machine>.rs`, give it a feature in
//! Cargo.toml and a line below. (A board build that overlays its own `platform.rs` replaces this file and must supply the same names.)
#[cfg(feature = "qemu-virt")]
mod qemu_virt;
#[cfg(feature = "qemu-virt")]
pub use qemu_virt::*;

#[cfg(not(feature = "qemu-virt"))]
compile_error!("select a platform feature (see kernel/src/platform.rs)");
