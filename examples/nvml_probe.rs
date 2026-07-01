// Diagnostic: does NVML allow GPU fan control at the current privilege level?
use nvml_wrapper::Nvml;

fn main() {
    let nvml = match Nvml::init() {
        Ok(n) => n,
        Err(e) => { println!("Nvml::init error: {e}"); return; }
    };
    let count = nvml.device_count().unwrap_or(0);
    println!("device_count = {count}");
    for g in 0..count {
        let mut dev = match nvml.device_by_index(g) { Ok(d) => d, Err(e) => { println!("dev {g} err {e}"); continue; } };
        println!("[{g}] {}", dev.name().unwrap_or_default());
        println!("  num_fans   = {:?}", dev.num_fans());
        println!("  fan_speed0 = {:?}", dev.fan_speed(0));
        match dev.set_fan_speed(0, 50) {
            Ok(_) => println!("  set_fan_speed(0,50) = OK"),
            Err(e) => println!("  set_fan_speed(0,50) = ERR: {e}"),
        }
        match dev.set_default_fan_speed(0) {
            Ok(_) => println!("  set_default_fan_speed(0) = OK (restored)"),
            Err(e) => println!("  set_default_fan_speed(0) = ERR: {e}"),
        }
    }
}
