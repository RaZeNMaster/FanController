/// Windows hardware fan control
///
/// System fans (motherboard headers) are controlled via the WinRing0 kernel
/// driver which gives ring-0 I/O port access, then communicating directly
/// with the SuperIO chip on the motherboard.
///
/// Supported SuperIO chip families:
///   • Nuvoton NCT677x  — NCT6775/6776/6779/6791/6792/6793/6795/6796/6797/6798
///                         (most ASUS, MSI, Gigabyte boards)
///   • Winbond W836xx   — W83627/W83667/W83677 (older boards, same driver as NCT)
///   • ITE IT87xx       — IT8783/8790/8792/8795/8620/8628/8665 (ASRock mainly)
///   • Fintek F71xxx    — F71808/F71858/F71882/F71889 (some Gigabyte boards)
///
/// WinRing0 driver is opened from the standard device name installed by
/// LibreHardwareMonitor, or from WinRing0x64.sys placed next to the exe.

use crate::hardware::{FanInfo, FanType, FanMode};
use crate::hardware::controller::FanBackend;

use std::ffi::c_void;
use windows::Win32::Foundation::{HANDLE, CloseHandle};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL,
    FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Services::{
    OpenSCManagerW, CreateServiceW, OpenServiceW, StartServiceW,
    CloseServiceHandle, SC_MANAGER_ALL_ACCESS, SERVICE_KERNEL_DRIVER,
    SERVICE_DEMAND_START, SERVICE_ERROR_NORMAL, SERVICE_ALL_ACCESS,
};
use windows::core::{PCWSTR, w};

// WinRing0x64.sys embedded at build time (downloaded by build.rs from LHM releases).
// Empty slice = download failed during build → Ring0 unavailable, falls back to WMI.
static WINRING0_DRIVER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "\\WinRing0x64.sys"));

// ─── WinRing0 IOCTL codes (OpenLibSys / LibreHardwareMonitor Ring0) ──────────
// CTL_CODE(OLS_TYPE=40000, function, METHOD_BUFFERED, access):
//   READ_IO_PORT_BYTE  = CTL_CODE(40000, 0x833, BUFFERED, FILE_READ_ACCESS)  = 0x9C4060CC
//   WRITE_IO_PORT_BYTE = CTL_CODE(40000, 0x836, BUFFERED, FILE_WRITE_ACCESS) = 0x9C40A0D8
// (The previous values were swapped/incorrect: reads were rejected by the driver
//  and "writes" actually used the read ioctl, so no I/O port write ever happened —
//  SuperIO detection always failed and fans fell back to read-only WMI.)
const IOCTL_READ_IO_PORT_BYTE:  u32 = 0x9C4060CC;
const IOCTL_WRITE_IO_PORT_BYTE: u32 = 0x9C40A0D8;

// ─── Chip family IDs ──────────────────────────────────────────────────────────
// Nuvoton NCT677x / Winbond W836xx (classic ISA I/O port access)
const ID_NCT6775: u16 = 0xB470;
const ID_NCT6776: u16 = 0xC330;
const ID_NCT6779: u16 = 0xC560;
const ID_NCT6791: u16 = 0xC800;
const ID_NCT6792: u16 = 0xC910;
const ID_NCT6793: u16 = 0xD120;
const ID_NCT6795: u16 = 0xD350;
const ID_NCT6796: u16 = 0xD420;
const ID_NCT6797: u16 = 0xD450;
// NCT6796D (0xD420) and NCT6798D (0xD42B) share the 0xD42x family after the
// revision nibble is masked off, so they are matched by ID_NCT6796.
// Nuvoton NCT6687D — newer chip (B660/Z690/B760/Z790 boards), 16-bit register addresses
const ID_NCT6687: u16 = 0xD592;
const ID_W83627DHG: u16 = 0xA020;
const ID_W83627EHF: u16 = 0x8800;
const ID_W83667HG:  u16 = 0xA510;
const ID_W83677HG:  u16 = 0xB350;
// ITE IT87xx
const ID_IT8620: u16 = 0x8620;
const ID_IT8628: u16 = 0x8628;
const ID_IT8665: u16 = 0x8665;
const ID_IT8686: u16 = 0x8686;
const ID_IT8688: u16 = 0x8688;
const ID_IT8783: u16 = 0x8783;
const ID_IT8790: u16 = 0x8790;
const ID_IT8792: u16 = 0x8792;
const ID_IT8795: u16 = 0x8795;
// Fintek F71xxx
const ID_F71808: u16 = 0x0504;
const ID_F71858: u16 = 0x0507;
const ID_F71862: u16 = 0x0601;
const ID_F71869: u16 = 0x0814;
const ID_F71882: u16 = 0x0541;
const ID_F71889: u16 = 0x0723;

// ─── SuperIO probe ports ──────────────────────────────────────────────────────
const SIO_PORTS: [u16; 2] = [0x2E, 0x4E];

// ═══════════════════════════════════════════════════════════════════════════════
// RING-0 DRIVER INTERFACE
// ═══════════════════════════════════════════════════════════════════════════════

struct Ring0 {
    handle: HANDLE,
}

unsafe impl Send for Ring0 {}
unsafe impl Sync for Ring0 {}

impl Ring0 {
    /// Try the device name that LHM installs, then try to install from file.
    fn open() -> Option<Self> {
        // 1. Already installed by LibreHardwareMonitor or previous run
        if let Some(r) = Self::open_device(w!("\\\\.\\WinRing0_1_2_0")) {
            eprintln!("[Ring0] Opened existing \\\\.\\WinRing0_1_2_0 device.");
            return Some(r);
        }
        eprintln!("[Ring0] Device not open yet — attempting to install driver ({} bytes embedded)...",
            WINRING0_DRIVER.len());
        // 2. Try installing from a driver file next to the exe
        if let Some(r) = Self::install_and_open() {
            eprintln!("[Ring0] Installed and opened WinRing0 driver.");
            return Some(r);
        }
        eprintln!("[Ring0] FAILED to open WinRing0 driver. Fan control unavailable \
                   (need Administrator; on Win11 HVCI/Memory-Integrity blocks unsigned WinRing0).");
        None
    }

    fn open_device(name: PCWSTR) -> Option<Self> {
        let handle = unsafe {
            CreateFileW(
                name,
                0xC000_0000u32, // GENERIC_READ | GENERIC_WRITE
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                HANDLE::default(),
            )
        };
        match handle {
            Ok(h) if !h.is_invalid() => Some(Self { handle: h }),
            _ => None,
        }
    }

    /// Extract the embedded driver to %TEMP%, install it as a Windows service, open it.
    /// Requires Administrator privileges. Returns None if not elevated or install fails.
    fn install_and_open() -> Option<Self> {
        // No embedded driver (build-time download failed)
        if WINRING0_DRIVER.len() < 10_000 {
            return None;
        }

        // Extract to %TEMP%\WinRing0x64.sys
        let temp = std::env::var("TEMP")
            .or_else(|_| std::env::var("TMP"))
            .unwrap_or_else(|_| "C:\\Windows\\Temp".into());
        let driver_path = format!("{temp}\\WinRing0x64.sys");

        std::fs::write(&driver_path, WINRING0_DRIVER).ok()?;

        let mut driver_w: Vec<u16> = driver_path.encode_utf16().chain([0u16]).collect();
        let name_w:    Vec<u16> = "WinRing0_1_2_0\0".encode_utf16().collect();
        let display_w: Vec<u16> = "WinRing0 1.2.0\0".encode_utf16().collect();

        unsafe {
            let scm = OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS).ok()?;

            // Create service (ignore error when already exists)
            let _ = CreateServiceW(
                scm,
                PCWSTR(name_w.as_ptr()),
                PCWSTR(display_w.as_ptr()),
                SERVICE_ALL_ACCESS,
                SERVICE_KERNEL_DRIVER,
                SERVICE_DEMAND_START,
                SERVICE_ERROR_NORMAL,
                PCWSTR(driver_w.as_ptr()),
                None, None, None, None, None,
            );

            // Open and start service
            if let Ok(svc) = OpenServiceW(scm, PCWSTR(name_w.as_ptr()), SERVICE_ALL_ACCESS) {
                let _ = StartServiceW(svc, None);
                let _ = CloseServiceHandle(svc);
            }
            let _ = CloseServiceHandle(scm);
        }

        std::thread::sleep(std::time::Duration::from_millis(150));
        Self::open_device(w!("\\\\.\\WinRing0_1_2_0"))
    }

    fn read_byte(&self, port: u16) -> Option<u8> {
        let port32 = port as u32;
        let mut out: u8 = 0;
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_READ_IO_PORT_BYTE,
                Some(&port32 as *const u32 as *const c_void),
                4,
                Some(&mut out as *mut u8 as *mut c_void),
                1,
                Some(&mut returned),
                None,
            )
        };
        ok.ok().map(|_| out)
    }

    fn write_byte(&self, port: u16, value: u8) -> bool {
        #[repr(C, packed)]
        struct WriteIn { port: u32, value: u8 }
        let input = WriteIn { port: port as u32, value };
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_WRITE_IO_PORT_BYTE,
                Some(&input as *const WriteIn as *const c_void),
                std::mem::size_of::<WriteIn>() as u32,
                None, 0,
                Some(&mut returned),
                None,
            )
        }.is_ok()
    }
}

impl Drop for Ring0 {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            unsafe { let _ = CloseHandle(self.handle); }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SUPERIO CHIP DETECTION AND REGISTER ACCESS
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, PartialEq, Debug)]
enum ChipFamily { Nuvoton, Nct6687, Ite, Fintek }

#[derive(Clone)]
struct SuperIoChip {
    family:   ChipFamily,
    chip_id:  u16,
    chip_name: &'static str,
    sio_port: u16,   // 0x2E or 0x4E
    iobase:   u16,   // hardware monitor base I/O address
    fan_count: usize,
    /// NCT6779D and newer (chip family ≥ 0xC56x) use a completely different
    /// register map (13-bit fan-count regs 0x4Bx, PWM regs 0x0xx/0xA09/0xB09)
    /// than the legacy NCT6775/6776/W836xx count-register scheme.
    modern: bool,
}

impl SuperIoChip {
    /// Probe both SIO ports; return first chip found.
    fn detect(r: &Ring0) -> Option<Self> {
        for &port in &SIO_PORTS {
            // Log chip ID for debugging even if not in our list
            r.write_byte(port, 0x87);
            r.write_byte(port, 0x87);
            let id = Self::sio_read_id(r, port);
            r.write_byte(port, 0xAA);
            if id != 0x0000 && id != 0xFFFF {
                eprintln!("[SuperIO] Port 0x{port:02X}: chip ID = 0x{id:04X}");
            }

            if let Some(chip) = Self::probe_nuvoton(r, port)
                .or_else(|| Self::probe_ite(r, port))
                .or_else(|| Self::probe_fintek(r, port))
            {
                eprintln!("[SuperIO] Detected: {} at port 0x{port:02X}, IOBASE=0x{:04X}",
                    chip.chip_name, chip.iobase);
                return Some(chip);
            }
        }
        eprintln!("[SuperIO] No supported chip found at 0x2E or 0x4E.");
        None
    }

    // ── Nuvoton / Winbond ──────────────────────────────────────────────────

    fn probe_nuvoton(r: &Ring0, port: u16) -> Option<Self> {
        // Enter extended function mode
        r.write_byte(port, 0x87);
        r.write_byte(port, 0x87);

        let chip_id = Self::sio_read_id(r, port);

        // Exit
        r.write_byte(port, 0xAA);

        // NCT6687D has a completely different register interface — handle separately
        // (mask the revision nibble: the chip reports e.g. 0xD59x).
        if chip_id & 0xFFF0 == ID_NCT6687 & 0xFFF0 {
            r.write_byte(port, 0x87);
            r.write_byte(port, 0x87);
            let iobase = Self::sio_get_iobase(r, port, 0x0B);
            r.write_byte(port, 0xAA);
            if iobase < 0x100 { return None; }
            return Some(SuperIoChip {
                family: ChipFamily::Nct6687, chip_id, chip_name: "NCT6687D",
                sio_port: port, iobase, fan_count: 8, modern: true,
            });
        }

        // Chip IDs carry a hardware revision in the low nibble (e.g. NCT6798D
        // reports 0xD42B, not 0xD428). Mask it off before matching the family,
        // otherwise an exact compare misses every real chip.
        let (name, fan_count) = match chip_id & 0xFFF0 {
            ID_NCT6775   => ("NCT6775F",       5),
            ID_NCT6776   => ("NCT6776F",       5),
            ID_NCT6779   => ("NCT6779D",       5),
            ID_NCT6791   => ("NCT6791D",       6),
            ID_NCT6792   => ("NCT6792D",       6),
            ID_NCT6793   => ("NCT6793D",       6),
            ID_NCT6795   => ("NCT6795D",       6),
            ID_NCT6796   => ("NCT6796D/6798D", 7), // 0xD42x incl. NCT6798D (0xD42B)
            ID_NCT6797   => ("NCT6797D",       7),
            ID_W83627DHG => ("W83627DHG",      3),
            ID_W83627EHF => ("W83627EHF",      5),
            ID_W83667HG  => ("W83667HG",       5),
            ID_W83677HG  => ("W83677HG",       5),
            _ => return None,
        };

        // Re-enter to read IOBASE
        r.write_byte(port, 0x87);
        r.write_byte(port, 0x87);
        let iobase = Self::sio_get_iobase(r, port, 0x0B); // LDN 11 = HW monitor
        r.write_byte(port, 0xAA);

        if iobase < 0x100 { return None; }

        // NCT6779D (0xC56x) and newer use the modern register map.
        let modern = (chip_id & 0xFFF0) >= 0xC560;
        Some(SuperIoChip { family: ChipFamily::Nuvoton, chip_id, chip_name: name, sio_port: port, iobase, fan_count, modern })
    }

    // ── ITE IT87xx ────────────────────────────────────────────────────────

    fn probe_ite(r: &Ring0, port: u16) -> Option<Self> {
        // ITE entry sequence
        r.write_byte(port, 0x87);
        r.write_byte(port, 0x01);
        r.write_byte(port, 0x55);
        r.write_byte(port, if port == 0x2E { 0x55 } else { 0xAA });

        let chip_id = Self::sio_read_id(r, port);

        // Exit
        r.write_byte(port, 0x02);
        r.write_byte(port + 1, 0x02);

        let (name, fan_count) = match chip_id {
            ID_IT8620 => ("IT8620E", 5),
            ID_IT8628 => ("IT8628E", 5),
            ID_IT8665 => ("IT8665E", 5),
            ID_IT8686 => ("IT8686E", 5),
            ID_IT8688 => ("IT8688E", 5),
            ID_IT8783 => ("IT8783E", 5),
            ID_IT8790 => ("IT8790E", 5),
            ID_IT8792 => ("IT8792E", 5),
            ID_IT8795 => ("IT8795E", 5),
            _ => return None,
        };

        // Re-enter to read IOBASE
        r.write_byte(port, 0x87);
        r.write_byte(port, 0x01);
        r.write_byte(port, 0x55);
        r.write_byte(port, if port == 0x2E { 0x55 } else { 0xAA });
        let iobase = Self::sio_get_iobase(r, port, 0x04); // LDN 4 = HW monitor
        r.write_byte(port, 0x02);
        r.write_byte(port + 1, 0x02);

        if iobase < 0x100 { return None; }

        Some(SuperIoChip { family: ChipFamily::Ite, chip_id, chip_name: name, sio_port: port, iobase, fan_count, modern: false })
    }

    // ── Fintek F71xxx ─────────────────────────────────────────────────────

    fn probe_fintek(r: &Ring0, port: u16) -> Option<Self> {
        // Fintek entry
        r.write_byte(port, 0x87);
        r.write_byte(port, 0x87);
        r.write_byte(port, 0x87);

        let chip_id = Self::sio_read_id(r, port);

        // Exit
        r.write_byte(port, 0xAA);

        let (name, fan_count) = match chip_id {
            ID_F71808 => ("F71808E", 3),
            ID_F71858 => ("F71858",  3),
            ID_F71862 => ("F71869A", 3),
            ID_F71869 => ("F71869",  3),
            ID_F71882 => ("F71882FG",3),
            ID_F71889 => ("F71889FG",3),
            _ => return None,
        };

        // Re-enter to read IOBASE
        r.write_byte(port, 0x87);
        r.write_byte(port, 0x87);
        r.write_byte(port, 0x87);
        let iobase = Self::sio_get_iobase(r, port, 0x04); // LDN 4 = HW monitor
        r.write_byte(port, 0xAA);

        if iobase < 0x100 { return None; }

        Some(SuperIoChip { family: ChipFamily::Fintek, chip_id, chip_name: name, sio_port: port, iobase, fan_count, modern: false })
    }

    // ── SIO helper: read 16-bit chip ID ──────────────────────────────────

    fn sio_read_id(r: &Ring0, port: u16) -> u16 {
        r.write_byte(port, 0x20);
        let hi = r.read_byte(port + 1).unwrap_or(0);
        r.write_byte(port, 0x21);
        let lo = r.read_byte(port + 1).unwrap_or(0);
        ((hi as u16) << 8) | lo as u16
    }

    /// Read 16-bit IOBASE from SuperIO LDN registers 0x60/0x61.
    fn sio_get_iobase(r: &Ring0, port: u16, ldn: u8) -> u16 {
        // Select LDN
        r.write_byte(port, 0x07);
        r.write_byte(port + 1, ldn);
        // Enable LDN (reg 0x30, bit 0)
        r.write_byte(port, 0x30);
        r.write_byte(port + 1, 0x01);
        // Read IOBASE
        r.write_byte(port, 0x60);
        let hi = r.read_byte(port + 1).unwrap_or(0);
        r.write_byte(port, 0x61);
        let lo = r.read_byte(port + 1).unwrap_or(0);
        ((hi as u16) << 8) | lo as u16
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// PER-FAMILY HARDWARE MONITOR ACCESS
// ═══════════════════════════════════════════════════════════════════════════════

/// Nuvoton NCT677x / Winbond W836xx register helpers.
/// Registers are accessed via IOBASE+5 (index) and IOBASE+6 (data).
/// Bank selection via writing to register 0x4E.
struct Nuvoton;

impl Nuvoton {
    const IDX: u16 = 5; // IOBASE offset for index register
    const DAT: u16 = 6; // IOBASE offset for data register

    fn read_reg(r: &Ring0, iobase: u16, bank: u8, reg: u8) -> Option<u8> {
        // Select bank
        r.write_byte(iobase + Self::IDX, 0x4E);
        r.write_byte(iobase + Self::DAT, bank);
        // Read register
        r.write_byte(iobase + Self::IDX, reg);
        r.read_byte(iobase + Self::DAT)
    }

    fn write_reg(r: &Ring0, iobase: u16, bank: u8, reg: u8, val: u8) {
        r.write_byte(iobase + Self::IDX, 0x4E);
        r.write_byte(iobase + Self::DAT, bank);
        r.write_byte(iobase + Self::IDX, reg);
        r.write_byte(iobase + Self::DAT, val);
    }

    // ── Modern NCT6779D+ register map (16-bit addresses) ────────────────────
    // A 16-bit address ABCD selects bank 0xAB (written to index reg 0x4E) and
    // register 0xCD. Layout taken from LibreHardwareMonitor (Nct677X.cs).
    const M_FAN_COUNT: [u16; 7] = [0x4B0, 0x4B2, 0x4B4, 0x4B6, 0x4B8, 0x4BA, 0x4CC];
    const M_PWM_OUT:   [u16; 7] = [0x001, 0x003, 0x011, 0x013, 0x015, 0xA09, 0xB09];
    const M_PWM_CMD:   [u16; 7] = [0x109, 0x209, 0x309, 0x809, 0x909, 0xA09, 0xB09];
    const M_CTRL_MODE: [u16; 7] = [0x102, 0x202, 0x302, 0x802, 0x902, 0xA02, 0xB02];
    const M_MAX_COUNT: u32 = 0x1FFF; // 13-bit counter
    const M_MIN_COUNT: u32 = 0x15;

    fn read_addr(r: &Ring0, iobase: u16, addr: u16) -> Option<u8> {
        Self::read_reg(r, iobase, (addr >> 8) as u8, (addr & 0xFF) as u8)
    }

    fn write_addr(r: &Ring0, iobase: u16, addr: u16, val: u8) {
        Self::write_reg(r, iobase, (addr >> 8) as u8, (addr & 0xFF) as u8, val);
    }

    /// Fan RPM. Legacy chips use a 12-bit counter at bank 0 regs 0x28-0x33;
    /// NCT6779D+ uses 13-bit counters at the 0x4Bx/0x4CC addresses.
    fn read_rpm(r: &Ring0, iobase: u16, fan: usize, modern: bool) -> Option<u32> {
        if modern {
            let addr = *Self::M_FAN_COUNT.get(fan)?;
            let hi = Self::read_addr(r, iobase, addr)? as u32;
            let lo = Self::read_addr(r, iobase, addr + 1)? as u32;
            // 13-bit count: high byte = bits 12..5, low byte = bits 4..0.
            let count = (hi << 5) | (lo & 0x1F);
            if count >= Self::M_MAX_COUNT || count < Self::M_MIN_COUNT { Some(0) }
            else { Some(1_350_000 / count) }
        } else {
            let hi_reg = 0x28u8 + (fan as u8) * 2;
            let hi = Self::read_reg(r, iobase, 0, hi_reg)? as u32;
            let lo = Self::read_reg(r, iobase, 0, hi_reg + 1)? as u32;
            let count = (hi << 8) | lo;
            if count == 0 || count == 0xFFFF { Some(0) }
            else { Some(1_350_000 / count) }
        }
    }

    /// Current PWM output as 0–100 %.
    fn read_pwm(r: &Ring0, iobase: u16, fan: usize, modern: bool) -> Option<u8> {
        let raw = if modern {
            Self::read_addr(r, iobase, *Self::M_PWM_OUT.get(fan)?)?
        } else {
            Self::read_reg(r, iobase, (fan + 1) as u8, 0x09)?
        };
        Some((raw as u32 * 100 / 255) as u8)
    }

    /// Read the raw fan-control-mode register (used to save the BIOS default
    /// before switching a fan to manual mode). Modern chips only.
    fn read_ctrl_mode(r: &Ring0, iobase: u16, fan: usize) -> Option<u8> {
        Self::read_addr(r, iobase, *Self::M_CTRL_MODE.get(fan)?)
    }

    /// Set PWM to pct % and switch fan to manual duty-cycle mode.
    fn set_pwm(r: &Ring0, iobase: u16, fan: usize, pct: u8, modern: bool) {
        let raw = (pct as u32 * 255 / 100).min(255) as u8;
        if modern {
            if let (Some(&mode_addr), Some(&cmd_addr)) =
                (Self::M_CTRL_MODE.get(fan), Self::M_PWM_CMD.get(fan))
            {
                Self::write_addr(r, iobase, mode_addr, 0x00); // 0 = manual mode
                Self::write_addr(r, iobase, cmd_addr, raw);
            }
        } else {
            let bank = (fan + 1) as u8;
            Self::write_reg(r, iobase, bank, 0x02, 0x00);
            Self::write_reg(r, iobase, bank, 0x09, raw);
        }
    }

    /// Hand the fan back to automatic control. `restore` is the fan-control-mode
    /// byte captured before the first manual write (BIOS/SmartFan default).
    fn reset_pwm(r: &Ring0, iobase: u16, fan: usize, modern: bool, restore: Option<u8>) {
        if modern {
            if let Some(&mode_addr) = Self::M_CTRL_MODE.get(fan) {
                // Restore the saved default; fall back to SmartFan mode (0x04)
                // if nothing was captured (never controlled this session).
                Self::write_addr(r, iobase, mode_addr, restore.unwrap_or(0x04));
            }
        } else {
            let bank = (fan + 1) as u8;
            Self::write_reg(r, iobase, bank, 0x02, restore.unwrap_or(0x03));
        }
    }
}

/// ITE IT87xx register helpers.
/// Direct register access: write index to IOBASE+5, read/write data from IOBASE+6.
/// No bank switching needed.
struct Ite;

impl Ite {
    const IDX: u16 = 5;
    const DAT: u16 = 6;

    fn read(r: &Ring0, iobase: u16, reg: u8) -> Option<u8> {
        r.write_byte(iobase + Self::IDX, reg);
        r.read_byte(iobase + Self::DAT)
    }

    fn write(r: &Ring0, iobase: u16, reg: u8, val: u8) {
        r.write_byte(iobase + Self::IDX, reg);
        r.write_byte(iobase + Self::DAT, val);
    }

    /// Fan RPM from 16-bit counter. Fans 1-5 at regs 0x0D-0x11 (older) or
    /// extended regs for fan4/5 on newer chips.
    fn read_rpm(r: &Ring0, iobase: u16, fan: usize) -> Option<u32> {
        // IT8783+ uses 16-bit tachometer registers
        let (hi_reg, lo_reg): (u8, u8) = match fan {
            0 => (0x0D, 0x18), // Fan1 high at 0x0D, low ext at 0x18
            1 => (0x0E, 0x19),
            2 => (0x0F, 0x1A),
            3 => (0x80, 0x81), // Fan4 extended
            4 => (0x82, 0x83), // Fan5 extended
            _ => return None,
        };
        let hi = Self::read(r, iobase, hi_reg)? as u32;
        let lo = Self::read(r, iobase, lo_reg)? as u32;
        let count = (hi << 8) | lo;
        if count == 0 || count >= 0xFFFF { Some(0) }
        else { Some(1_500_000 / count) }
    }

    /// Current PWM duty cycle % (0–100). PWM registers: 0x6B–0x6F for fans 1–5.
    fn read_pwm(r: &Ring0, iobase: u16, fan: usize) -> Option<u8> {
        let reg = 0x6Bu8 + fan as u8;
        let raw = Self::read(r, iobase, reg)? as u32;
        // IT87xx stores 0–127 as 0–100 % (127 = full speed)
        Some((raw * 100 / 127).min(100) as u8)
    }

    /// Set fan to manual PWM mode and write duty cycle.
    fn set_pwm(r: &Ring0, iobase: u16, fan: usize, pct: u8) {
        // Disable SmartGuardian for this fan (reg 0x15, one bit per fan)
        if let Some(ctrl) = Self::read(r, iobase, 0x15) {
            let bit = 1u8 << (5 + fan.min(2)); // bits 5/6/7 for fans 1/2/3
            Self::write(r, iobase, 0x15, ctrl & !bit);
        }
        let raw = (pct as u32 * 127 / 100).min(127) as u8;
        Self::write(r, iobase, 0x6Bu8 + fan as u8, raw);
    }

    /// Re-enable SmartGuardian automatic control.
    fn reset_pwm(r: &Ring0, iobase: u16, fan: usize) {
        if let Some(ctrl) = Self::read(r, iobase, 0x15) {
            let bit = 1u8 << (5 + fan.min(2));
            Self::write(r, iobase, 0x15, ctrl | bit);
        }
    }
}

/// Fintek F71xxx register helpers.
/// Similar to Nuvoton but with different register addresses.
struct Fintek;

impl Fintek {
    fn read(r: &Ring0, iobase: u16, page: u8, reg: u8) -> Option<u8> {
        // Page select at base+4
        r.write_byte(iobase + 4, page);
        r.write_byte(iobase + 5, reg);
        r.read_byte(iobase + 6)
    }

    fn write(r: &Ring0, iobase: u16, page: u8, reg: u8, val: u8) {
        r.write_byte(iobase + 4, page);
        r.write_byte(iobase + 5, reg);
        r.write_byte(iobase + 6, val);
    }

    fn read_rpm(r: &Ring0, iobase: u16, fan: usize) -> Option<u32> {
        let reg = 0xA0u8 + fan as u8 * 0x10;
        let hi = Self::read(r, iobase, 0, reg)? as u32;
        let lo = Self::read(r, iobase, 0, reg + 1)? as u32;
        let count = (hi << 8) | lo;
        if count == 0 || count == 0xFFFF { Some(0) }
        else { Some(1_500_000 / count) }
    }

    fn read_pwm(r: &Ring0, iobase: u16, fan: usize) -> Option<u8> {
        // Page 0, PWM duty regs 0xAA, 0xBA, 0xCA for fans 1-3
        let reg = 0xAAu8 + fan as u8 * 0x10;
        let raw = Self::read(r, iobase, 0, reg)? as u32;
        Some((raw * 100 / 255).min(100) as u8)
    }

    fn set_pwm(r: &Ring0, iobase: u16, fan: usize, pct: u8) {
        let reg = 0xAAu8 + fan as u8 * 0x10;
        // Fan mode reg: 0xA2, 0xB2, 0xC2 — set to manual (bit 1:0 = 01)
        let mode_reg = 0xA2u8 + fan as u8 * 0x10;
        if let Some(m) = Self::read(r, iobase, 0, mode_reg) {
            Self::write(r, iobase, 0, mode_reg, (m & 0xFC) | 0x01);
        }
        Self::write(r, iobase, 0, reg, (pct as u32 * 255 / 100).min(255) as u8);
    }

    fn reset_pwm(r: &Ring0, iobase: u16, fan: usize) {
        // Set mode back to auto temperature control (bit 1:0 = 11)
        let mode_reg = 0xA2u8 + fan as u8 * 0x10;
        if let Some(m) = Self::read(r, iobase, 0, mode_reg) {
            Self::write(r, iobase, 0, mode_reg, m | 0x03);
        }
    }
}

/// NCT6687D register helpers.
/// This chip uses 16-bit register addresses (high byte → IOBASE+4, low → IOBASE+5, data → IOBASE+6).
/// Used on Intel B660/Z690/B760/Z790/B860/Z890 generation boards.
///
/// Register map (from Linux kernel nct6687 driver and LibreHardwareMonitor Nct6687.cs):
///   Fan speed (16-bit RPM count): 0x1400 + fan*2 (hi), 0x1401 + fan*2 (lo)
///   Fan PWM duty cycle (0–255):   0x1000 + fan
///   Fan mode register:             0x1020 + fan  (0x00 = auto, 0xFF = manual)
struct Nct6687;

impl Nct6687 {
    fn read(r: &Ring0, iobase: u16, reg: u16) -> Option<u8> {
        r.write_byte(iobase + 4, (reg >> 8) as u8);
        r.write_byte(iobase + 5, reg as u8);
        r.read_byte(iobase + 6)
    }

    fn write(r: &Ring0, iobase: u16, reg: u16, val: u8) {
        r.write_byte(iobase + 4, (reg >> 8) as u8);
        r.write_byte(iobase + 5, reg as u8);
        r.write_byte(iobase + 6, val);
    }

    /// Fan RPM from 13-bit count at 0x1400 + fan*2.
    fn read_rpm(r: &Ring0, iobase: u16, fan: usize) -> Option<u32> {
        let base = 0x1400u16 + fan as u16 * 2;
        let hi = Self::read(r, iobase, base)? as u32;
        let lo = Self::read(r, iobase, base + 1)? as u32;
        let count = ((hi << 8) | lo) & 0x1FFF;
        if count == 0 || count >= 0x1FFF { Some(0) }
        else { Some(1_350_000 / count) }
    }

    /// Current PWM % (0–100) from register 0x1000 + fan.
    fn read_pwm(r: &Ring0, iobase: u16, fan: usize) -> Option<u8> {
        let raw = Self::read(r, iobase, 0x1000 + fan as u16)?;
        Some((raw as u32 * 100 / 255) as u8)
    }

    /// Set fan to manual mode and apply duty cycle.
    fn set_pwm(r: &Ring0, iobase: u16, fan: usize, pct: u8) {
        let pwm = (pct.max(20) as u32 * 255 / 100).min(255) as u8;
        // 0x1020 + fan: fan mode — 0xFF = manual override
        Self::write(r, iobase, 0x1020 + fan as u16, 0xFF);
        Self::write(r, iobase, 0x1000 + fan as u16, pwm);
    }

    /// Return fan to automatic (SmartFan) mode.
    fn reset_pwm(r: &Ring0, iobase: u16, fan: usize) {
        Self::write(r, iobase, 0x1020 + fan as u16, 0x00);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SYSTEM FAN BACKEND
// ═══════════════════════════════════════════════════════════════════════════════

pub struct SystemFanBackend {
    ring0: Option<Ring0>,
    chip:  Option<SuperIoChip>,
    /// BIOS/SmartFan control-mode byte per fan index, captured before the first
    /// manual write so `reset_to_auto` can hand the fan back to the firmware.
    saved_modes: std::collections::HashMap<usize, u8>,
}

impl SystemFanBackend {
    /// True only if the driver opened AND a real chip was detected with working I/O.
    /// If only the driver opened but chip detection failed (e.g. VBS blocking I/O reads),
    /// this returns false so the WMI fallback backend activates.
    pub fn has_chip(&self) -> bool { self.chip.is_some() }
}

unsafe impl Send for SystemFanBackend {}
unsafe impl Sync for SystemFanBackend {}

impl SystemFanBackend {
    pub fn new() -> Self {
        let ring0 = Ring0::open();
        let chip = ring0.as_ref().and_then(SuperIoChip::detect);
        Self { ring0, chip, saved_modes: std::collections::HashMap::new() }
    }

    fn fan_id(fan: usize) -> String { format!("sio:{fan}") }

    fn fan_label(&self, fan: usize) -> String {
        let chip = self.chip.as_ref().map(|c| c.chip_name).unwrap_or("Unknown");
        format!("{chip} Fan {}", fan + 1)
    }

    fn read_rpm(&self, fan: usize) -> Option<u32> {
        let r = self.ring0.as_ref()?;
        let c = self.chip.as_ref()?;
        match c.family {
            ChipFamily::Nuvoton => Nuvoton::read_rpm(r, c.iobase, fan, c.modern),
            ChipFamily::Nct6687 => Nct6687::read_rpm(r, c.iobase, fan),
            ChipFamily::Ite     => Ite::read_rpm(r, c.iobase, fan),
            ChipFamily::Fintek  => Fintek::read_rpm(r, c.iobase, fan),
        }
    }

    fn read_pwm(&self, fan: usize) -> Option<u8> {
        let r = self.ring0.as_ref()?;
        let c = self.chip.as_ref()?;
        match c.family {
            ChipFamily::Nuvoton => Nuvoton::read_pwm(r, c.iobase, fan, c.modern),
            ChipFamily::Nct6687 => Nct6687::read_pwm(r, c.iobase, fan),
            ChipFamily::Ite     => Ite::read_pwm(r, c.iobase, fan),
            ChipFamily::Fintek  => Fintek::read_pwm(r, c.iobase, fan),
        }
    }
}

/// Standalone hardware diagnostics for the `--diag` CLI flag.
/// Run from an **elevated** terminal: `fancontroller.exe --diag`
/// Prints exactly where SuperIO fan control succeeds or fails, plus raw
/// RPM/PWM values so wrong readings can be traced to the register level.
pub fn run_diagnostics() {
    eprintln!("═══ FanController Windows diagnostics ═══");
    eprintln!("Embedded WinRing0 driver size: {} bytes (need > 10000)", WINRING0_DRIVER.len());

    let ring0 = match Ring0::open() {
        Some(r) => r,
        None => {
            eprintln!("RESULT: WinRing0 driver could not be opened → no direct fan control.");
            eprintln!("  • Are you running as Administrator?");
            eprintln!("  • Win11: disable Core Isolation → Memory Integrity, or install LibreHardwareMonitor.");
            return;
        }
    };

    // Probe both SIO ports and report every chip ID seen, even unsupported ones.
    for &port in &SIO_PORTS {
        ring0.write_byte(port, 0x87);
        ring0.write_byte(port, 0x87);
        let id = SuperIoChip::sio_read_id(&ring0, port);
        ring0.write_byte(port, 0xAA);
        eprintln!("Port 0x{port:02X}: raw chip ID = 0x{id:04X} (family nibble 0x{:04X})", id & 0xFFF0);
    }

    match SuperIoChip::detect(&ring0) {
        None => {
            eprintln!("RESULT: driver works but no SUPPORTED SuperIO chip matched.");
            eprintln!("  Send the raw chip IDs above so the chip can be added.");
        }
        Some(chip) => {
            eprintln!("RESULT: detected {} (family {:?}) at IOBASE 0x{:04X}, {} fan headers.",
                chip.chip_name, chip.family, chip.iobase, chip.fan_count);
            let backend = SystemFanBackend { ring0: Some(ring0), chip: Some(chip.clone()), saved_modes: std::collections::HashMap::new() };
            for i in 0..chip.fan_count {
                let rpm = backend.read_rpm(i);
                let pwm = backend.read_pwm(i);
                eprintln!("  Fan {i}: RPM={rpm:?}  PWM={pwm:?}%");
            }
            eprintln!("If RPM/PWM look wrong (all 0, all 100, or absurd), the register map for this chip needs adjusting.");
        }
    }
    eprintln!("════════════════════════════════════════");
}

impl FanBackend for SystemFanBackend {
    fn name(&self) -> &str { "superio" }

    fn scan(&mut self) -> Vec<FanInfo> {
        let chip = match &self.chip { Some(c) => c.clone(), None => return Vec::new() };
        let mut fans = Vec::new();
        let mut fan_num = 0u32;
        let mut pump_num = 0u32;

        for i in 0..chip.fan_count {
            let rpm = self.read_rpm(i);
            // Skip empty headers (0 RPM or no reading)
            match rpm { Some(0) | None => continue, _ => {} }

            // Pump heuristic: very high RPM compared to typical fans (> 1800 RPM
            // with less than 10% PWM variation from max often indicates a pump).
            // For now we use a simple high-RPM threshold — user can rename later.
            let rpm_val = rpm.unwrap_or(0);
            let is_pump = rpm_val > 2500
                && self.read_pwm(i).map_or(false, |p| p > 90);

            let label = if is_pump {
                pump_num += 1;
                format!("Pump {pump_num}")
            } else {
                fan_num += 1;
                format!("Fan {fan_num}")
            };

            fans.push(FanInfo {
                id: Self::fan_id(i),
                label,
                fan_type: FanType::System,
                mode: FanMode::Auto,
                rpm,
                speed_pct: self.read_pwm(i),
                temp_c: None,
                curve: Vec::new(),
                rpm_min: Some(0),
                rpm_max: None,
                controllable: true,
                is_pump,
            });
        }
        fans
    }

    fn set_speed_pct(&mut self, fan_id: &str, pct: u8) -> anyhow::Result<()> {
        if !fan_id.starts_with("sio:") { anyhow::bail!("not a superio fan"); }
        let fan: usize = fan_id.trim_start_matches("sio:").parse()?;
        let r = self.ring0.as_ref().ok_or_else(|| anyhow::anyhow!("WinRing0 driver not available"))?;
        let c = self.chip.as_ref().ok_or_else(|| anyhow::anyhow!("No SuperIO chip detected"))?;
        let (family, iobase, modern) = (c.family, c.iobase, c.modern);
        let pct = pct.max(20); // safety floor
        // Capture the BIOS/SmartFan control-mode byte once, before overriding it,
        // so reset_to_auto can hand the fan back to the firmware (modern Nuvoton).
        if family == ChipFamily::Nuvoton && modern && !self.saved_modes.contains_key(&fan) {
            if let Some(m) = Nuvoton::read_ctrl_mode(r, iobase, fan) {
                self.saved_modes.insert(fan, m);
            }
        }
        match family {
            ChipFamily::Nuvoton => Nuvoton::set_pwm(r, iobase, fan, pct, modern),
            ChipFamily::Nct6687 => Nct6687::set_pwm(r, iobase, fan, pct),
            ChipFamily::Ite     => Ite::set_pwm(r, iobase, fan, pct),
            ChipFamily::Fintek  => Fintek::set_pwm(r, iobase, fan, pct),
        }
        Ok(())
    }

    fn reset_to_auto(&mut self, fan_id: &str) -> anyhow::Result<()> {
        if !fan_id.starts_with("sio:") { anyhow::bail!("not a superio fan"); }
        let fan: usize = fan_id.trim_start_matches("sio:").parse()?;
        let r = self.ring0.as_ref().ok_or_else(|| anyhow::anyhow!("WinRing0 driver not available"))?;
        let c = self.chip.as_ref().ok_or_else(|| anyhow::anyhow!("No SuperIO chip detected"))?;
        let (family, iobase, modern) = (c.family, c.iobase, c.modern);
        let restore = self.saved_modes.get(&fan).copied();
        match family {
            ChipFamily::Nuvoton => Nuvoton::reset_pwm(r, iobase, fan, modern, restore),
            ChipFamily::Nct6687 => Nct6687::reset_pwm(r, iobase, fan),
            ChipFamily::Ite     => Ite::reset_pwm(r, iobase, fan),
            ChipFamily::Fintek  => Fintek::reset_pwm(r, iobase, fan),
        }
        self.saved_modes.remove(&fan);
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// WMI / LHM BACKEND  (read-only, used when Ring0 is unavailable)
// ═══════════════════════════════════════════════════════════════════════════════

use serde::Deserialize;
use wmi::{COMLibrary, WMIConnection};

#[derive(Deserialize, Debug)]
struct LhmSensor {
    #[serde(rename = "Name")]       name: String,
    #[serde(rename = "Value")]      value: f32,
    #[serde(rename = "SensorType")] sensor_type: String,
    #[serde(rename = "Identifier")] identifier: String,
    #[serde(rename = "Parent")]     parent: String,
}

#[derive(Deserialize, Debug)]
struct LhmHardware {
    #[serde(rename = "Name")]         name: String,
    #[serde(rename = "Identifier")]   identifier: String,
    #[serde(rename = "HardwareType")] hw_type: String,
}

pub struct WmiBackend;

impl WmiBackend {
    pub fn new() -> Self { Self }

    fn connect() -> Option<WMIConnection> {
        for ns in &["ROOT\\LibreHardwareMonitor", "ROOT\\OpenHardwareMonitor"] {
            if let Ok(com) = COMLibrary::new() {
                if let Ok(conn) = WMIConnection::with_namespace_path(ns, com) {
                    return Some(conn);
                }
            }
        }
        None
    }
}

impl FanBackend for WmiBackend {
    fn name(&self) -> &str { "wmi_lhm" }

    fn scan(&mut self) -> Vec<FanInfo> {
        let conn = match Self::connect() { Some(c) => c, None => return Vec::new() };

        // Load hardware list so we can filter out GPU hardware
        let hardware: Vec<LhmHardware> = conn
            .raw_query("SELECT Name, Identifier, HardwareType FROM Hardware")
            .unwrap_or_default();

        // Collect identifiers of GPU hardware — those fans are handled by NVIDIA/AMD backends
        let gpu_parents: std::collections::HashSet<String> = hardware.iter()
            .filter(|h| {
                let t = h.hw_type.to_ascii_lowercase();
                t.contains("gpu") || t.contains("nvidia") || t.contains("ati")
            })
            .map(|h| h.identifier.to_ascii_lowercase())
            .collect();

        let sensors: Vec<LhmSensor> = match conn.raw_query(
            "SELECT Name, Value, SensorType, Identifier, Parent FROM Sensor \
             WHERE SensorType = 'Fan'"
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let controls: Vec<LhmSensor> = conn
            .raw_query(
                "SELECT Name, Value, SensorType, Identifier, Parent FROM Sensor \
                 WHERE SensorType = 'Control'"
            )
            .unwrap_or_default();

        let mut fan_num = 0u32;
        let mut pump_num = 0u32;

        sensors.into_iter()
            // Skip GPU fans — those show up via NvidiaBackend / AmdBackend
            .filter(|s| !gpu_parents.contains(&s.parent.to_ascii_lowercase()))
            // Skip zero-RPM sensors (empty headers)
            .filter(|s| s.value.round() as u32 > 0)
            .map(|s| {
                let duty = controls.iter()
                    .find(|c| c.parent.eq_ignore_ascii_case(&s.parent))
                    .map(|c| c.value.clamp(0.0, 100.0) as u8);

                // Detect pumps by sensor name (LHM labels them "Pump" explicitly)
                let name_lower = s.name.to_ascii_lowercase();
                let is_pump = name_lower.contains("pump")
                    || name_lower.contains("aio")
                    || name_lower.contains("w_pump")
                    || name_lower.contains("water");

                let label = if is_pump {
                    pump_num += 1;
                    format!("Pump {pump_num}")
                } else {
                    fan_num += 1;
                    format!("Fan {fan_num}")
                };

                FanInfo {
                    id: format!("wmi:{}", s.identifier),
                    label,
                    fan_type: FanType::System,
                    mode: FanMode::Auto,
                    rpm: Some(s.value.round() as u32),
                    speed_pct: duty,
                    temp_c: None,
                    curve: Vec::new(),
                    rpm_min: None,
                    rpm_max: None,
                    controllable: false,
                    is_pump,
                }
            })
            .collect()
    }

    fn set_speed_pct(&mut self, _: &str, _: u8) -> anyhow::Result<()> {
        anyhow::bail!("WMI fan monitoring is read-only. Use Ring0 driver for control.")
    }

    fn reset_to_auto(&mut self, _: &str) -> anyhow::Result<()> {
        anyhow::bail!("WMI fan monitoring is read-only.")
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// NVIDIA NVML BACKEND
// ═══════════════════════════════════════════════════════════════════════════════

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;

pub struct NvidiaBackend {
    nvml: Option<Nvml>,
    // Private NVAPI for actual fan control (NVML control is blocked on Windows).
    nvapi: Option<crate::hardware::nvapi::NvApi>,
}

impl NvidiaBackend {
    pub fn new() -> Self {
        Self { nvml: Nvml::init().ok(), nvapi: crate::hardware::nvapi::NvApi::new() }
    }

    fn parse_id(fan_id: &str) -> anyhow::Result<(u32, u32)> {
        if !fan_id.starts_with("nvidia:") { anyhow::bail!("not nvidia"); }
        let rest = fan_id.trim_start_matches("nvidia:");
        let mut p = rest.splitn(2, ':');
        Ok((p.next().unwrap_or("0").parse()?, p.next().unwrap_or("0").parse()?))
    }
}

impl FanBackend for NvidiaBackend {
    fn name(&self) -> &str { "nvidia_nvml" }

    fn scan(&mut self) -> Vec<FanInfo> {
        let nvml = match &self.nvml { Some(n) => n, None => return Vec::new() };
        let mut fans = Vec::new();
        for g in 0..nvml.device_count().unwrap_or(0) {
            let dev = match nvml.device_by_index(g) { Ok(d) => d, Err(_) => continue };
            let name = dev.name().unwrap_or_else(|_| "NVIDIA GPU".into());
            let temp = dev.temperature(TemperatureSensor::Gpu).ok().map(|t| t as f32);
            let fc = dev.num_fans().unwrap_or(1).max(1);
            for f in 0..fc {
                let label = if fc == 1 { format!("{name} Fan") } else { format!("{name} Fan {}", f + 1) };
                fans.push(FanInfo {
                    id: format!("nvidia:{g}:{f}"),
                    label, fan_type: FanType::Nvidia, mode: FanMode::Auto,
                    rpm: dev.fan_speed_rpm(f).ok(),
                    speed_pct: dev.fan_speed(f).ok().map(|s| s.min(100) as u8),
                    temp_c: temp, curve: Vec::new(),
                    rpm_min: Some(0), rpm_max: Some(3500),
                    controllable: true, is_pump: false,
                });
            }
        }
        fans
    }

    fn set_speed_pct(&mut self, fan_id: &str, pct: u8) -> anyhow::Result<()> {
        let (g, _f) = Self::parse_id(fan_id)?;
        // Control via private NVAPI (public NVML control is blocked on Windows).
        let api = self.nvapi.as_ref().ok_or_else(|| anyhow::anyhow!(
            "NVAPI nicht verfügbar (nvapi64.dll fehlt oder keine NVIDIA-GPU). \
             GPU-Lüftersteuerung ist unter Windows nur über NVAPI möglich."))?;
        api.set_fans(g as usize, Some(pct.clamp(30, 100)))
    }

    fn reset_to_auto(&mut self, fan_id: &str) -> anyhow::Result<()> {
        let (g, _) = Self::parse_id(fan_id)?;
        let api = self.nvapi.as_ref().ok_or_else(|| anyhow::anyhow!(
            "NVAPI nicht verfügbar — GPU-Lüfter können nicht zurückgesetzt werden."))?;
        api.set_fans(g as usize, None)
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// AMD ADL BACKEND
// ═══════════════════════════════════════════════════════════════════════════════

use libloading::{Library, Symbol};

#[repr(C)] #[derive(Clone, Copy, Default)]
struct AdlFanSpeedValue { i_size: i32, i_speed_type: i32, i_fan_speed: i32, i_flags: i32 }

#[repr(C)] #[derive(Clone, Copy, Default)]
struct AdlTemperature { i_size: i32, i_temperature: i32 }

#[repr(C)] #[derive(Clone, Copy, Default)]
struct AdlOdnFanControl {
    i_mode: i32, i_fan_control_mode: i32, i_current_fan_speed_mode: i32,
    i_current_fan_speed: i32, i_target_fan_speed: i32, i_target_temperature: i32,
    i_min_performance_clock: i32, i_max_fan_speed: i32, i_min_fan_speed: i32,
    i_auto_system_command_fan_speed: i32,
}

const ADL_MAX_PATH: usize = 256;
#[repr(C)]
struct AdlAdapterInfo {
    i_size: i32, i_adapter_index: i32, str_udid: [u8; ADL_MAX_PATH],
    i_bus_number: i32, i_device_number: i32, i_function_number: i32, i_vendor_id: i32,
    str_adapter_name: [u8; ADL_MAX_PATH], str_display_name: [u8; ADL_MAX_PATH],
    i_present: i32, i_exist: i32,
    str_driver_path: [u8; ADL_MAX_PATH], str_driver_path_ext: [u8; ADL_MAX_PATH],
    str_pnp_string: [u8; ADL_MAX_PATH], i_os_display_index: i32,
}

type FnCreate    = unsafe extern "C" fn(cb: extern "C" fn(i32) -> *mut c_void, c: i32, ctx: *mut *mut c_void) -> i32;
type FnDestroy   = unsafe extern "C" fn(ctx: *mut c_void) -> i32;
type FnNumAdap   = unsafe extern "C" fn(ctx: *mut c_void, n: *mut i32) -> i32;
type FnAdapInfo  = unsafe extern "C" fn(ctx: *mut c_void, info: *mut AdlAdapterInfo, sz: i32) -> i32;
type FnFanGet    = unsafe extern "C" fn(ctx: *mut c_void, a: i32, t: i32, v: *mut AdlFanSpeedValue) -> i32;
type FnFanSet    = unsafe extern "C" fn(ctx: *mut c_void, a: i32, t: i32, v: *mut AdlFanSpeedValue) -> i32;
type FnFanDef    = unsafe extern "C" fn(ctx: *mut c_void, a: i32, t: i32) -> i32;
type FnTempGet   = unsafe extern "C" fn(ctx: *mut c_void, a: i32, t: i32, v: *mut AdlTemperature) -> i32;
type FnOdnGet    = unsafe extern "C" fn(ctx: *mut c_void, a: i32, v: *mut AdlOdnFanControl) -> i32;
type FnOdnSet    = unsafe extern "C" fn(ctx: *mut c_void, a: i32, v: *mut AdlOdnFanControl) -> i32;

extern "C" fn adl_malloc(sz: i32) -> *mut c_void {
    let layout = std::alloc::Layout::array::<u8>(sz as usize)
        .unwrap_or(std::alloc::Layout::new::<u8>());
    unsafe { std::alloc::alloc_zeroed(layout) as *mut c_void }
}

#[derive(Clone)]
struct AdlAdapter { index: i32, name: String }

pub struct AmdBackend { lib: Option<Library>, ctx: *mut c_void, adapters: Vec<AdlAdapter> }

unsafe impl Send for AmdBackend {}
unsafe impl Sync for AmdBackend {}

impl AmdBackend {
    pub fn new() -> Self {
        Self::try_init().unwrap_or(Self { lib: None, ctx: std::ptr::null_mut(), adapters: Vec::new() })
    }

    fn try_init() -> Option<Self> {
        let lib = unsafe {
            Library::new("atiadlxx.dll").or_else(|_| Library::new("atiadlxy.dll")).ok()?
        };
        let mut ctx: *mut c_void = std::ptr::null_mut();
        unsafe {
            let create: Symbol<FnCreate> = lib.get(b"ADL2_Main_Control_Create\0").ok()?;
            if create(adl_malloc, 1, &mut ctx) != 0 { return None; }

            let num_fn: Symbol<FnNumAdap> = lib.get(b"ADL2_Adapter_NumberOfAdapters_Get\0").ok()?;
            let mut num = 0i32;
            num_fn(ctx, &mut num);
            if num == 0 { return None; }

            let mut info_buf: Vec<AdlAdapterInfo> = (0..num).map(|_| AdlAdapterInfo {
                i_size: std::mem::size_of::<AdlAdapterInfo>() as i32,
                i_adapter_index: 0, str_udid: [0; ADL_MAX_PATH],
                i_bus_number: 0, i_device_number: 0, i_function_number: 0, i_vendor_id: 0,
                str_adapter_name: [0; ADL_MAX_PATH], str_display_name: [0; ADL_MAX_PATH],
                i_present: 0, i_exist: 0,
                str_driver_path: [0; ADL_MAX_PATH], str_driver_path_ext: [0; ADL_MAX_PATH],
                str_pnp_string: [0; ADL_MAX_PATH], i_os_display_index: 0,
            }).collect();

            if let Ok(f) = lib.get::<Symbol<FnAdapInfo>>(b"ADL2_Adapter_AdapterInfo_Get\0") {
                f(ctx, info_buf.as_mut_ptr(), (num as usize * std::mem::size_of::<AdlAdapterInfo>()) as i32);
            }

            let adapters = info_buf.iter()
                .filter(|a| a.i_present != 0 && a.i_vendor_id == 0x1002)
                .map(|a| {
                    let name = std::ffi::CStr::from_ptr(a.str_adapter_name.as_ptr() as *const i8)
                        .to_string_lossy().trim().to_string();
                    let name = if name.is_empty() { format!("AMD GPU {}", a.i_adapter_index) } else { name };
                    AdlAdapter { index: a.i_adapter_index, name }
                })
                .collect();

            Some(Self { lib: Some(lib), ctx, adapters })
        }
    }

    fn read_fan_pct(&self, lib: &Library, adap: i32) -> Option<u8> {
        unsafe {
            if let Ok(f) = lib.get::<Symbol<FnOdnGet>>(b"ADL2_OverdriveN_FanControl_Get\0") {
                let mut c = AdlOdnFanControl::default();
                if f(self.ctx, adap, &mut c) == 0 && c.i_current_fan_speed > 0 {
                    return Some(c.i_current_fan_speed.clamp(0, 100) as u8);
                }
            }
            if let Ok(f) = lib.get::<Symbol<FnFanGet>>(b"ADL2_Overdrive5_FanSpeed_Get\0") {
                let mut v = AdlFanSpeedValue { i_size: 16, i_speed_type: 2, ..Default::default() };
                if f(self.ctx, adap, 0, &mut v) == 0 {
                    return Some(v.i_fan_speed.clamp(0, 100) as u8);
                }
            }
        }
        None
    }

    fn read_temp(&self, lib: &Library, adap: i32) -> Option<f32> {
        unsafe {
            if let Ok(f) = lib.get::<Symbol<FnTempGet>>(b"ADL2_Overdrive5_Temperature_Get\0") {
                let mut t = AdlTemperature { i_size: 8, ..Default::default() };
                if f(self.ctx, adap, 0, &mut t) == 0 && t.i_temperature > 0 {
                    return Some(if t.i_temperature > 1000 { t.i_temperature as f32 / 1000.0 } else { t.i_temperature as f32 });
                }
            }
        }
        None
    }

    fn apply_pct(&self, lib: &Library, adap: i32, pct: u8) -> anyhow::Result<()> {
        let pct = pct.clamp(20, 100) as i32;
        unsafe {
            if let (Ok(gf), Ok(sf)) = (
                lib.get::<Symbol<FnOdnGet>>(b"ADL2_OverdriveN_FanControl_Get\0"),
                lib.get::<Symbol<FnOdnSet>>(b"ADL2_OverdriveN_FanControl_Set\0"),
            ) {
                let mut c = AdlOdnFanControl::default();
                if gf(self.ctx, adap, &mut c) == 0 {
                    c.i_mode = 1; c.i_target_fan_speed = pct;
                    if sf(self.ctx, adap, &mut c) == 0 { return Ok(()); }
                }
            }
            if let Ok(f) = lib.get::<Symbol<FnFanSet>>(b"ADL2_Overdrive5_FanSpeed_Set\0") {
                let mut v = AdlFanSpeedValue { i_size: 16, i_speed_type: 2, i_fan_speed: pct, i_flags: 1 };
                if f(self.ctx, adap, 0, &mut v) == 0 { return Ok(()); }
            }
        }
        anyhow::bail!("ADL fan control failed for adapter {adap}")
    }

    fn reset_fan(&self, lib: &Library, adap: i32) -> anyhow::Result<()> {
        unsafe {
            if let (Ok(gf), Ok(sf)) = (
                lib.get::<Symbol<FnOdnGet>>(b"ADL2_OverdriveN_FanControl_Get\0"),
                lib.get::<Symbol<FnOdnSet>>(b"ADL2_OverdriveN_FanControl_Set\0"),
            ) {
                let mut c = AdlOdnFanControl::default();
                if gf(self.ctx, adap, &mut c) == 0 {
                    c.i_mode = 0;
                    let _ = sf(self.ctx, adap, &mut c);
                    return Ok(());
                }
            }
            if let Ok(f) = lib.get::<Symbol<FnFanDef>>(b"ADL2_Overdrive5_FanSpeedToDefault_Set\0") {
                if f(self.ctx, adap, 0) == 0 { return Ok(()); }
            }
        }
        anyhow::bail!("ADL reset failed")
    }
}

impl Drop for AmdBackend {
    fn drop(&mut self) {
        if let Some(ref lib) = self.lib {
            if !self.ctx.is_null() {
                unsafe {
                    if let Ok(f) = lib.get::<Symbol<FnDestroy>>(b"ADL2_Main_Control_Destroy\0") {
                        f(self.ctx);
                    }
                }
                self.ctx = std::ptr::null_mut();
            }
        }
    }
}

impl FanBackend for AmdBackend {
    fn name(&self) -> &str { "amd_adl" }

    fn scan(&mut self) -> Vec<FanInfo> {
        let lib = match &self.lib { Some(l) => l, None => return Vec::new() };
        if self.ctx.is_null() { return Vec::new(); }
        self.adapters.iter().map(|a| {
            FanInfo {
                id: format!("amd:{}", a.index),
                label: format!("{} Fan", a.name),
                fan_type: FanType::Amd, mode: FanMode::Auto,
                rpm: None,
                speed_pct: self.read_fan_pct(lib, a.index),
                temp_c: self.read_temp(lib, a.index),
                curve: Vec::new(), rpm_min: Some(0), rpm_max: None,
                controllable: true, is_pump: false,
            }
        }).collect()
    }

    fn set_speed_pct(&mut self, fan_id: &str, pct: u8) -> anyhow::Result<()> {
        if !fan_id.starts_with("amd:") { anyhow::bail!("not amd"); }
        let idx: i32 = fan_id.trim_start_matches("amd:").parse()?;
        let lib = self.lib.as_ref().ok_or_else(|| anyhow::anyhow!("ADL not loaded"))?;
        self.apply_pct(lib, idx, pct)
    }

    fn reset_to_auto(&mut self, fan_id: &str) -> anyhow::Result<()> {
        if !fan_id.starts_with("amd:") { anyhow::bail!("not amd"); }
        let idx: i32 = fan_id.trim_start_matches("amd:").parse()?;
        let lib = self.lib.as_ref().ok_or_else(|| anyhow::anyhow!("ADL not loaded"))?;
        self.reset_fan(lib, idx)
    }
}
