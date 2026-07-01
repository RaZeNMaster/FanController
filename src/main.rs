mod hardware;
mod ipc;

#[cfg(target_os = "linux")]
mod gpu_nvml;

use hardware::FanController;
use ipc::IpcMessage;

use std::sync::{Arc, Mutex};
use serde_json::json;
use tao::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

#[cfg(target_os = "linux")]
const DAEMON_PID_FILE: &str = "/tmp/fancontroller-daemon.pid";

fn main() -> anyhow::Result<()> {
    // Linux only: privileged NVML subcommands re-exec via sudo -n, then exit.
    #[cfg(target_os = "linux")]
    if let Some(code) = gpu_nvml::run_cli() {
        std::process::exit(code);
    }

    // Linux only: background daemon mode (apply saved curves without GUI).
    #[cfg(target_os = "linux")]
    if std::env::args().any(|a| a == "--daemon") {
        return run_daemon();
    }

    // Linux only: stop any running daemon so the GUI can take over.
    #[cfg(target_os = "linux")]
    stop_daemon();

    // Linux only: one-time udev/sudoers setup.
    #[cfg(target_os = "linux")]
    if !is_root() && needs_setup() {
        setup_permissions();
    }

    // Windows: install WinRing0 driver if not present (requires admin, asks via UAC).
    #[cfg(target_os = "windows")]
    maybe_install_driver();

    // Windows: start LibreHardwareMonitor in background for hardware monitoring.
    #[cfg(target_os = "windows")]
    ensure_lhm_running();

    let controller = Arc::new(Mutex::new(FanController::new()));

    let event_loop = EventLoopBuilder::<String>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title("FanController")
        .with_inner_size(tao::dpi::LogicalSize::new(1100u32, 750u32))
        .with_min_inner_size(tao::dpi::LogicalSize::new(800u32, 600u32))
        .build(&event_loop)?;

    let controller_ipc = Arc::clone(&controller);
    let webview = WebViewBuilder::new(&window)
        .with_url("app://localhost/")
        .with_custom_protocol("app".into(), |_request| {
            let html = include_str!("../assets/index.html");
            wry::http::Response::builder()
                .header("Content-Type", "text/html; charset=utf-8")
                .body(std::borrow::Cow::Borrowed(html.as_bytes()))
                .unwrap()
        })
        .with_ipc_handler(move |request: wry::http::Request<String>| {
            handle_ipc_message(request.body(), Arc::clone(&controller_ipc));
        })
        .build()?;

    // Background thread: scan hardware every 3 s, push fan data to GUI every 1 s.
    let controller_bg = Arc::clone(&controller);
    std::thread::spawn(move || {
        let mut ticks = 0u32;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(1000));
            let mut ctrl = controller_bg.lock().unwrap();
            if ticks % 3 == 0 { ctrl.scan_all(); }
            ctrl.tick();
            ticks += 1;
            let fans = ctrl.get_all_fans();
            let custom_profile = ctrl.get_custom_profile().clone();
            let payload = json!({
                "type": "fan_update",
                "fans": fans,
                "custom_profile": custom_profile,
            });
            proxy.send_event(payload.to_string()).ok();
        }
    });

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(json) => {
                let js = format!("window.__rustUpdate({})", json);
                webview.evaluate_script(&js).ok();
            }
            _ => {}
        }
    });
}

fn is_controllable(ctrl: &FanController, fan_id: &str) -> bool {
    ctrl.get_all_fans().iter()
        .find(|f| f.id == fan_id)
        .map(|f| f.controllable)
        .unwrap_or(false)
}

fn handle_ipc_message(raw: &str, controller: Arc<Mutex<FanController>>) {
    let msg: IpcMessage = match serde_json::from_str(raw) {
        Ok(m) => m,
        Err(e) => { eprintln!("[IPC] Parse error: {e}"); return; }
    };

    match msg {
        IpcMessage::SetCurve { fan_id, points } => {
            let mut ctrl = controller.lock().unwrap();
            if !is_controllable(&ctrl, &fan_id) { return; }
            if let Err(e) = ctrl.set_curve(&fan_id, points) {
                eprintln!("[IPC] set_curve error: {e}");
            }
        }
        IpcMessage::SetSpeed { fan_id, rpm_percent } => {
            let mut ctrl = controller.lock().unwrap();
            if !is_controllable(&ctrl, &fan_id) { return; }
            if let Err(e) = ctrl.set_fixed_speed(&fan_id, rpm_percent) {
                eprintln!("[IPC] set_speed error: {e}");
            }
        }
        IpcMessage::ResetToDefault { fan_id } => {
            let mut ctrl = controller.lock().unwrap();
            if !is_controllable(&ctrl, &fan_id) { return; }
            ctrl.reset_to_default(&fan_id);
        }
        IpcMessage::Rescan => {
            let mut ctrl = controller.lock().unwrap();
            ctrl.scan_all();
        }
        IpcMessage::RenameFan { fan_id, label } => {
            let mut ctrl = controller.lock().unwrap();
            ctrl.rename_fan(&fan_id, label);
        }
        IpcMessage::ResetAll => {
            let mut ctrl = controller.lock().unwrap();
            let ids: Vec<String> = ctrl.get_all_fans().iter().map(|f| f.id.clone()).collect();
            for id in ids {
                ctrl.reset_to_default(&id);
            }
        }
        IpcMessage::SaveCustomProfile { fan_id, points } => {
            let mut ctrl = controller.lock().unwrap();
            ctrl.save_custom_curve(&fan_id, points);
        }
    }
}

// ─── Windows: WinRing0 driver auto-install ────────────────────────────────────

#[cfg(target_os = "windows")]
fn winring0_device_exists() -> bool {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    let path: Vec<u16> = OsStr::new("\\\\.\\WinRing0_1_2_0")
        .encode_wide().chain([0u16]).collect();
    let h = unsafe {
        windows::Win32::Storage::FileSystem::CreateFileW(
            windows::core::PCWSTR(path.as_ptr()),
            0xC000_0000u32,
            windows::Win32::Storage::FileSystem::FILE_SHARE_READ |
            windows::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
            None,
            windows::Win32::Storage::FileSystem::OPEN_EXISTING,
            windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
            windows::Win32::Foundation::HANDLE::default(),
        )
    };
    match h {
        Ok(handle) if !handle.is_invalid() => {
            unsafe { let _ = windows::Win32::Foundation::CloseHandle(handle); }
            true
        }
        _ => false,
    }
}

#[cfg(target_os = "windows")]
fn is_elevated() -> bool {
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut size = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        let _ = windows::Win32::Foundation::CloseHandle(token);
        ok.is_ok() && elevation.TokenIsElevated != 0
    }
}

/// Check if WinRing0 driver is installed. If not, re-launch this exe elevated
/// with --install-driver so Windows shows a single UAC prompt.
#[cfg(target_os = "windows")]
fn maybe_install_driver() {
    // --install-driver: called by the elevated re-launch, installs driver then exits.
    if std::env::args().any(|a| a == "--install-driver") {
        // The elevated process: just creating the FanController triggers Ring0 install.
        use hardware::FanController;
        let _ = FanController::new(); // Ring0::install_and_open() runs here
        std::process::exit(0);
    }

    if winring0_device_exists() {
        return; // Already installed, nothing to do
    }

    if is_elevated() {
        // Already admin (e.g. user ran as admin manually): install inline.
        return; // FanController::new() will handle it
    }

    // Not admin and driver missing: re-launch elevated for one-time install.
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };
    let exe_str = exe.to_string_lossy();
    let exe_w: Vec<u16> = exe_str.encode_utf16().chain([0u16]).collect();
    let args_w: Vec<u16> = "--install-driver\0".encode_utf16().collect();

    eprintln!("[FanController] Installing WinRing0 driver — Windows will ask for admin permission.");

    unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            windows::Win32::Foundation::HWND::default(),
            windows::core::w!("runas"),
            windows::core::PCWSTR(exe_w.as_ptr()),
            windows::core::PCWSTR(args_w.as_ptr()),
            None,
            windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
        );
    }

    // Wait briefly for the elevated process to install the driver, then continue.
    std::thread::sleep(std::time::Duration::from_secs(3));
}

// ─── Windows: LibreHardwareMonitor auto-start ─────────────────────────────────

/// Returns the directory where we store our bundled LHM copy.
#[cfg(target_os = "windows")]
fn lhm_dir() -> std::path::PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| "C:\\".into());
    std::path::PathBuf::from(appdata).join("FanController").join("lhm")
}

/// Check if LibreHardwareMonitor.exe is currently running.
#[cfg(target_os = "windows")]
fn lhm_is_running() -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq LibreHardwareMonitor.exe", "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("LibreHardwareMonitor"))
        .unwrap_or(false)
}

/// Find an existing LHM executable (our bundled copy or a system install).
#[cfg(target_os = "windows")]
fn find_lhm_exe() -> Option<std::path::PathBuf> {
    let candidates = [
        // Our own bundled copy (downloaded by us)
        lhm_dir().join("LibreHardwareMonitor.exe"),
        // User-installed LHM in Program Files
        std::path::PathBuf::from(
            std::env::var("ProgramFiles").unwrap_or_default()
        ).join("LibreHardwareMonitor").join("LibreHardwareMonitor.exe"),
        std::path::PathBuf::from(
            std::env::var("ProgramFiles(x86)").unwrap_or_default()
        ).join("LibreHardwareMonitor").join("LibreHardwareMonitor.exe"),
        // Common desktop / downloads locations
        std::path::PathBuf::from(
            std::env::var("USERPROFILE").unwrap_or_default()
        ).join("Desktop").join("LibreHardwareMonitor.exe"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

/// Download and extract LibreHardwareMonitor to our bundled LHM directory.
#[cfg(target_os = "windows")]
fn download_lhm() -> Option<std::path::PathBuf> {
    let dir = lhm_dir();
    let exe = dir.join("LibreHardwareMonitor.exe");
    if exe.exists() { return Some(exe); }

    eprintln!("[FanController] Downloading LibreHardwareMonitor...");
    let dir_str = dir.to_string_lossy();

    let script = format!(r#"
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try {{
    New-Item -ItemType Directory -Force '{dir_str}' | Out-Null
    $url = 'https://github.com/LibreHardwareMonitor/LibreHardwareMonitor/releases/download/v0.9.4/LibreHardwareMonitor-net472.zip'
    $zip = '{dir_str}\lhm.zip'
    Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing -TimeoutSec 90
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $z = [System.IO.Compression.ZipFile]::OpenRead($zip)
    foreach ($entry in $z.Entries) {{
        if ($entry.Name -eq '') {{ continue }}
        $dest = '{dir_str}\' + $entry.Name
        [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $dest, $true)
    }}
    $z.Dispose()
    Remove-Item $zip -Force -ErrorAction SilentlyContinue
    Write-Host 'OK'
}} catch {{ Write-Host "FAIL: $_" }}
"#);

    let out = std::process::Command::new("powershell")
        .args(["-NonInteractive", "-Command", &script])
        .output()
        .ok()?;

    if String::from_utf8_lossy(&out.stdout).contains("OK") && exe.exists() {
        eprintln!("[FanController] LibreHardwareMonitor downloaded.");
        Some(exe)
    } else {
        eprintln!("[FanController] LHM download failed: {}",
            String::from_utf8_lossy(&out.stdout).trim());
        None
    }
}

/// Launch LibreHardwareMonitor minimized to tray. Requests admin via UAC if needed
/// (LHM needs admin to install its hardware monitoring driver on first run).
#[cfg(target_os = "windows")]
fn launch_lhm(exe: &std::path::Path) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWMINIMIZED;
    use windows::Win32::Foundation::HWND;

    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, ShowWindow, SW_HIDE, SW_SHOWNORMAL};

    let exe_w: Vec<u16> = exe.to_string_lossy().encode_utf16().chain([0u16]).collect();

    unsafe {
        ShellExecuteW(
            HWND::default(),
            windows::core::w!("runas"),
            windows::core::PCWSTR(exe_w.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }

    // Hide the LHM window as soon as it appears (poll up to 15 seconds)
    std::thread::spawn(|| {
        let title: Vec<u16> = "LibreHardwareMonitor\0".encode_utf16().collect();
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            unsafe {
                if let Ok(hwnd) = FindWindowW(None, windows::core::PCWSTR(title.as_ptr())) {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    eprintln!("[FanController] LibreHardwareMonitor window hidden.");
                    return;
                }
            }
        }
    });

    eprintln!("[FanController] LibreHardwareMonitor started.");
}

/// Main entry point: ensure LHM is running so WMI fan data is available.
/// LHM is started asynchronously — the app opens immediately and fans appear
/// in the UI a few seconds after LHM initializes (background scan picks it up).
#[cfg(target_os = "windows")]
fn ensure_lhm_running() {
    if lhm_is_running() {
        eprintln!("[FanController] LibreHardwareMonitor already running.");
        return;
    }

    // Find or download LHM in a background thread so app startup isn't blocked
    std::thread::spawn(|| {
        let exe = find_lhm_exe().or_else(download_lhm);
        match exe {
            Some(path) => {
                launch_lhm(&path);
                eprintln!("[FanController] LibreHardwareMonitor started — \
                           system fans will appear in a few seconds.");
            }
            None => {
                eprintln!("[FanController] Could not find or download LibreHardwareMonitor. \
                           System fans unavailable.");
            }
        }
    });
}

// ─── Linux-only: daemon + root check + permissions setup ──────────────────────

#[cfg(target_os = "linux")]
pub fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .map(|uid| uid == "0")
        })
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn run_daemon() -> anyhow::Result<()> {
    if !is_root() && needs_setup() {
        setup_permissions();
    }

    std::fs::write(DAEMON_PID_FILE, std::process::id().to_string()).ok();

    let mut ctrl = FanController::new();
    ctrl.scan_all();

    let custom = ctrl.get_custom_profile().clone();
    for (fan_id, points) in custom {
        if points.len() >= 2 {
            let _ = ctrl.set_curve(&fan_id, points);
        }
    }

    let mut ticks = 0u32;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(1000));
        if ticks % 3 == 0 { ctrl.scan_all(); }
        ctrl.tick();
        ticks += 1;
    }
}

#[cfg(target_os = "linux")]
fn stop_daemon() {
    if let Ok(pid_str) = std::fs::read_to_string(DAEMON_PID_FILE) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        std::fs::remove_file(DAEMON_PID_FILE).ok();
    }
}

#[cfg(target_os = "linux")]
fn needs_setup() -> bool {
    !std::path::Path::new("/etc/udev/rules.d/60-fancontroller.rules").exists()
}

#[cfg(target_os = "linux")]
fn setup_permissions() {
    use std::io::Write as _;

    eprintln!("FanController: one-time setup — enter sudo password:");

    let user = std::env::var("USER").unwrap_or_else(|_| "ALL".into());
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/usr/local/bin/fancontroller".into());

    let ok = (|| -> anyhow::Result<()> {
        std::process::Command::new("sudo").args(["groupadd", "-f", "hwmon"]).status()?;
        std::process::Command::new("sudo").args(["usermod", "-aG", "hwmon", &user]).status()?;

        let udev_rule = "SUBSYSTEM==\"hwmon\", KERNEL==\"hwmon[0-9]*\", \
            ACTION==\"add\", GROUP=\"hwmon\", MODE=\"0660\"\n";
        let mut child = std::process::Command::new("sudo")
            .args(["tee", "/etc/udev/rules.d/60-fancontroller.rules"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()?;
        child.stdin.as_mut().unwrap().write_all(udev_rule.as_bytes())?;
        child.wait()?;

        std::process::Command::new("sudo")
            .args(["sh", "-c",
                "udevadm control --reload-rules && udevadm trigger --subsystem-match=hwmon"])
            .status()?;

        let sudoers = format!(
            "{user} ALL=(ALL) NOPASSWD: {exe} --gpu-set *\n\
             {user} ALL=(ALL) NOPASSWD: {exe} --gpu-reset *\n"
        );
        let mut child = std::process::Command::new("sudo")
            .args(["tee", "/etc/sudoers.d/fancontroller"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()?;
        child.stdin.as_mut().unwrap().write_all(sudoers.as_bytes())?;
        child.wait()?;
        std::process::Command::new("sudo")
            .args(["chmod", "0440", "/etc/sudoers.d/fancontroller"])
            .status()?;

        Ok(())
    })();

    match ok {
        Ok(()) => eprintln!(
            "Setup complete. NOTE: log out and back in once so your user joins the hwmon group."
        ),
        Err(e) => eprintln!("Setup failed ({e})."),
    }
}
