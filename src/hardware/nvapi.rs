//! Windows NVIDIA GPU fan control via the private NVAPI (`nvapi64.dll`).
//!
//! NVML's public fan-control API is rejected by consumer GeForce drivers on
//! Windows (`NVML_ERROR_NO_PERMISSION`), so — like MSI Afterburner and
//! LibreHardwareMonitor — we use the private NVAPI **ClientFanCoolers**
//! interface. Function interface IDs and struct layouts follow
//! LibreHardwareMonitor's `Interop/NvApi.cs`.
//!
//! This targets Turing/Ampere/Ada cards (RTX 20xx and newer). The legacy
//! `NvAPI_GPU_SetCoolerLevels` interface was removed on those GPUs.

use std::ffi::c_void;
use libloading::Library;

const NVAPI_MAX_PHYSICAL_GPUS: usize = 64;
const MAX_FAN_CONTROLLER_ITEMS: usize = 32;

// NVAPI function interface IDs (passed to nvapi_QueryInterface).
const ID_INITIALIZE:              u32 = 0x0150_E828;
const ID_ENUM_PHYSICAL_GPUS:      u32 = 0xE5AC_921F;
const ID_FAN_COOLERS_GET_CONTROL: u32 = 0x814B_209F;
const ID_FAN_COOLERS_SET_CONTROL: u32 = 0xA589_71A5;

const NVAPI_OK: i32 = 0;

type NvHandle = *mut c_void; // opaque NvPhysicalGpuHandle

type QueryInterfaceFn    = unsafe extern "C" fn(u32) -> *const c_void;
type InitializeFn        = unsafe extern "C" fn() -> i32;
type EnumPhysicalGpusFn  = unsafe extern "C" fn(*mut NvHandle, *mut u32) -> i32;
type FanCoolersControlFn = unsafe extern "C" fn(NvHandle, *mut NvFanCoolerControl) -> i32;

#[repr(C)]
#[derive(Clone, Copy)]
struct NvFanCoolerControlItem {
    cooler_id: u32,
    level: u32,        // 0–100 %
    control_mode: u32, // 0 = Auto, 1 = Manual
    reserved: [u32; 8],
}

#[repr(C)]
struct NvFanCoolerControl {
    version: u32,
    reserved: u32,
    count: u32,
    reserved2: [u32; 8],
    items: [NvFanCoolerControlItem; MAX_FAN_CONTROLLER_ITEMS],
}

impl NvFanCoolerControl {
    fn versioned() -> Self {
        // MAKE_NVAPI_VERSION(struct, 1) = sizeof | (1 << 16)
        let size = std::mem::size_of::<Self>() as u32;
        Self {
            version: size | (1u32 << 16),
            reserved: 0,
            count: 0,
            reserved2: [0; 8],
            items: [NvFanCoolerControlItem {
                cooler_id: 0, level: 0, control_mode: 0, reserved: [0; 8],
            }; MAX_FAN_CONTROLLER_ITEMS],
        }
    }
}

pub struct NvApi {
    _lib: Library, // keep the DLL loaded for the lifetime of the handles
    get_control: FanCoolersControlFn,
    set_control: FanCoolersControlFn,
    gpus: Vec<NvHandle>,
}

// NvApi holds raw driver handles that are only used behind &self; the
// underlying NVAPI is thread-safe for these read/modify/write cooler calls.
unsafe impl Send for NvApi {}
unsafe impl Sync for NvApi {}

impl NvApi {
    /// Load nvapi64.dll, initialise NVAPI and enumerate physical GPUs.
    /// Returns None if the DLL is missing or any step fails.
    pub fn new() -> Option<Self> {
        unsafe {
            let lib = Library::new("nvapi64.dll").ok()?;
            let query: libloading::Symbol<QueryInterfaceFn> =
                lib.get(b"nvapi_QueryInterface\0").ok()?;
            let query = *query;

            let init: InitializeFn = std::mem::transmute(non_null(query(ID_INITIALIZE))?);
            if init() != NVAPI_OK { return None; }

            let enum_gpus: EnumPhysicalGpusFn =
                std::mem::transmute(non_null(query(ID_ENUM_PHYSICAL_GPUS))?);
            let get_control: FanCoolersControlFn =
                std::mem::transmute(non_null(query(ID_FAN_COOLERS_GET_CONTROL))?);
            let set_control: FanCoolersControlFn =
                std::mem::transmute(non_null(query(ID_FAN_COOLERS_SET_CONTROL))?);

            let mut handles: [NvHandle; NVAPI_MAX_PHYSICAL_GPUS] =
                [std::ptr::null_mut(); NVAPI_MAX_PHYSICAL_GPUS];
            let mut count: u32 = 0;
            if enum_gpus(handles.as_mut_ptr(), &mut count) != NVAPI_OK { return None; }

            let gpus = handles[..(count as usize).min(NVAPI_MAX_PHYSICAL_GPUS)].to_vec();
            if gpus.is_empty() { return None; }

            Some(Self { _lib: lib, get_control, set_control, gpus })
        }
    }

    /// Set every fan cooler of `gpu` to `level` % in manual mode, or hand the
    /// coolers back to automatic driver control when `level` is `None`.
    pub fn set_fans(&self, gpu: usize, level: Option<u8>) -> anyhow::Result<()> {
        let handle = *self.gpus.get(gpu)
            .ok_or_else(|| anyhow::anyhow!("NVAPI: GPU index {gpu} out of range"))?;

        // Read current cooler control (also fills in count + cooler IDs).
        let mut ctrl = NvFanCoolerControl::versioned();
        let st = unsafe { (self.get_control)(handle, &mut ctrl) };
        if st != NVAPI_OK { anyhow::bail!("NVAPI ClientFanCoolersGetControl failed (status {st})"); }

        let n = (ctrl.count as usize).min(MAX_FAN_CONTROLLER_ITEMS);
        if n == 0 { anyhow::bail!("NVAPI reported no controllable GPU fan coolers"); }
        for item in ctrl.items[..n].iter_mut() {
            match level {
                Some(pct) => { item.control_mode = 1; item.level = pct.min(100) as u32; }
                None      => { item.control_mode = 0; item.level = 0; }
            }
        }

        let st = unsafe { (self.set_control)(handle, &mut ctrl) };
        if st != NVAPI_OK { anyhow::bail!("NVAPI ClientFanCoolersSetControl failed (status {st})"); }
        Ok(())
    }
}

#[inline]
fn non_null(p: *const c_void) -> Option<*const c_void> {
    if p.is_null() { None } else { Some(p) }
}
