//! PCI configuration mechanism #1 for the QEMU PC's conventional PCI bus.
//! No BAR relocation: the BIOS must have assigned a valid I/O window.
//! Address/data transactions hold CLI briefly because CF8 is shared state.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Address {
    pub bus: u8,
    pub slot: u8,
    pub function: u8,
}

impl Address {
    fn config_address(self, offset: u8) -> u32 {
        0x8000_0000 | (self.bus as u32) << 16
            | (self.slot as u32 & 31) << 11
            | (self.function as u32 & 7) << 8 | (offset as u32 & 0xfc)
    }
}

/// RTL8139's BAR0 is a 256-byte I/O window. Reject zero, MMIO, reserved
/// bit 1, misalignment, legacy low ports and a window beyond 16-bit I/O.
pub fn rtl8139_io_bar(bar: u32) -> Option<u16> {
    let base = bar & !3;
    (bar & 3 == 1 && base >= 0x100 && base <= 0xff00 && base & 0xff == 0)
        .then_some(base as u16)
}

#[cfg(not(test))]
unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    core::arch::asm!("in eax, dx", in("dx") port, out("eax") value,
                     options(nomem, nostack, preserves_flags));
    value
}

#[cfg(not(test))]
unsafe fn outl(port: u16, value: u32) {
    core::arch::asm!("out dx, eax", in("dx") port, in("eax") value,
                     options(nomem, nostack, preserves_flags));
}

#[cfg(not(test))]
pub fn read32(address: Address, offset: u8) -> u32 {
    crate::interrupts::without(|| unsafe {
        outl(0xcf8, address.config_address(offset));
        inl(0xcfc)
    })
}

/// Scan a finite 256 buses x 32 slots x at most 8 functions. No bridge
/// configuration changes are made; already enumerated buses are inspected.
#[cfg(not(test))]
pub fn find(vendor: u16, device: u16) -> Option<Address> {
    let wanted = (device as u32) << 16 | vendor as u32;
    for bus in 0..=255 {
        for slot in 0..32 {
            let address = Address { bus, slot, function: 0 };
            let id = read32(address, 0);
            if id as u16 == 0xffff { continue; }
            if id == wanted { return Some(address); }
            if read32(address, 0x0c) & (0x80 << 16) == 0 { continue; }
            for function in 1..8 {
                let address = Address { bus, slot, function };
                if read32(address, 0) == wanted { return Some(address); }
            }
        }
    }
    None
}

/// Enable I/O decoding and bus mastering, and disable the PCI interrupt
/// pin for this polling driver. Write only the command word: writing the
/// entire dword would accidentally acknowledge W1C status bits above it.
#[cfg(not(test))]
pub fn enable_io_bus_master(address: Address) -> bool {
    crate::interrupts::without(|| unsafe {
        outl(0xcf8, address.config_address(4));
        let command = (inl(0xcfc) as u16) | 0x0405;
        core::arch::asm!("out dx, ax", in("dx") 0xcfcu16, in("ax") command,
                         options(nomem, nostack, preserves_flags));
        inl(0xcfc) as u16 & 0x0405 == 0x0405
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_encoding_keeps_bus_slot_function_and_dword_offset() {
        assert_eq!(Address { bus: 0xab, slot: 0x1f, function: 7 }.config_address(0x13), 0x80ab_ff10);
        assert_eq!(Address { bus: 0, slot: 3, function: 0 }.config_address(4), 0x8000_1804);
    }

    #[test]
    fn only_assigned_in_range_aligned_io_windows_are_accepted() {
        assert_eq!(rtl8139_io_bar(0xc001), Some(0xc000));
        assert_eq!(rtl8139_io_bar(0xff01), Some(0xff00));
        for bar in [0, 1, 0xc000, 0xc003, 0xc081, 0x1_0001, 0xffff_ffff] {
            assert_eq!(rtl8139_io_bar(bar), None, "{bar:#x}");
        }
    }
}
