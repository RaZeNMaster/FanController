mod controller;
mod types;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "windows")]
mod nvapi;

pub use controller::FanController;
pub use types::{FanInfo, FanType, CurvePoint, FanMode};
