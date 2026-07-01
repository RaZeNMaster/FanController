// build.rs — Provides WinRing0x64.sys for embedding into the binary.
//
// Priority:
//   1. assets/WinRing0x64.sys  (already extracted, checked into repo or placed manually)
//   2. Download LibreHardwareMonitorLib.dll and extract the driver from it at build time
//   3. Write empty placeholder → app falls back to WMI read-only mode
//
// The driver is only needed on Windows. It is embedded via include_bytes! in windows.rs.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        // Non-Windows: write empty placeholder so include_bytes! compiles
        let out_dir = std::env::var("OUT_DIR").unwrap();
        std::fs::write(format!("{out_dir}\\WinRing0x64.sys"), b"").ok();
        return;
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let out_driver = format!("{out_dir}\\WinRing0x64.sys");

    println!("cargo:rerun-if-changed=assets/WinRing0x64.sys");
    println!("cargo:rerun-if-env-changed=WINRING0_PATH");

    // 1. Use pre-extracted driver from assets/ (fastest, works offline)
    if let Ok(meta) = std::fs::metadata("assets/WinRing0x64.sys") {
        if meta.len() > 10_000 {
            std::fs::copy("assets/WinRing0x64.sys", &out_driver).ok();
            eprintln!("cargo:warning=WinRing0x64.sys loaded from assets/ ({} bytes)", meta.len());
            return;
        }
    }

    // 2. Download LHM and extract driver from LibreHardwareMonitorLib.dll embedded resource
    eprintln!("cargo:warning=assets/WinRing0x64.sys not found — downloading from LibreHardwareMonitor...");

    let script = format!(r#"
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try {{
    $lhm_url  = 'https://github.com/LibreHardwareMonitor/LibreHardwareMonitor/releases/download/v0.9.4/LibreHardwareMonitor-net472.zip'
    $zip_path = '{out_dir}\lhm_build.zip'
    $dll_path = '{out_dir}\LibreHardwareMonitorLib.dll'
    $out_path = '{out_dir}\WinRing0x64.sys'

    # Download LHM zip
    Invoke-WebRequest -Uri $lhm_url -OutFile $zip_path -UseBasicParsing -TimeoutSec 120

    # Extract LibreHardwareMonitorLib.dll from zip
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($zip_path)
    $dll_entry = $zip.Entries | Where-Object {{ $_.Name -eq 'LibreHardwareMonitorLib.dll' }} | Select-Object -First 1
    [System.IO.Compression.ZipFileExtensions]::ExtractToFile($dll_entry, $dll_path, $true)
    $zip.Dispose()
    Remove-Item $zip_path -Force -ErrorAction SilentlyContinue

    # Extract WinRing0x64.sys from DLL embedded resource (stored as gzip with 1-byte prefix)
    $dll    = [Reflection.Assembly]::LoadFile($dll_path)
    $stream = $dll.GetManifestResourceStream('LibreHardwareMonitor.Resources.WinRing0x64.gz')
    $stream.ReadByte() | Out-Null  # skip LHM's 0xFF version prefix
    $gz     = New-Object System.IO.Compression.GZipStream($stream, [System.IO.Compression.CompressionMode]::Decompress)
    $fs     = [System.IO.File]::Create($out_path)
    $gz.CopyTo($fs)
    $fs.Close(); $gz.Close()

    # Copy to assets/ for future builds (skip network next time)
    $null = New-Item -ItemType Directory -Force 'assets'
    Copy-Item $out_path 'assets\WinRing0x64.sys' -Force
    Remove-Item $dll_path -Force -ErrorAction SilentlyContinue

    Write-Host "OK: extracted $((Get-Item $out_path).Length) bytes"
}} catch {{
    Write-Host "FAILED: $_"
    # Write empty placeholder so build does not fail
    [System.IO.File]::WriteAllBytes('{out_dir}\WinRing0x64.sys', @())
}}
"#);

    let output = std::process::Command::new("powershell")
        .args(["-NonInteractive", "-Command", &script])
        .output();

    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if stdout.contains("OK:") {
                eprintln!("cargo:warning=WinRing0x64.sys downloaded and embedded successfully.");
            } else {
                eprintln!("cargo:warning=WinRing0x64.sys download failed: {stdout}");
                eprintln!("cargo:warning=Fan control will be read-only. Place assets/WinRing0x64.sys manually to enable it.");
                std::fs::write(&out_driver, b"").ok();
            }
        }
        Err(e) => {
            eprintln!("cargo:warning=PowerShell failed: {e}");
            std::fs::write(&out_driver, b"").ok();
        }
    }
}
