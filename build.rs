/// Rebuild when `cfg.toml` changes, and pass the ESP-IDF environment through to rustc.
fn main() {
    println!("cargo:rerun-if-changed=cfg.toml");
    // Include paths, linker script, and the ESP-IDF build flags.
    embuild::espidf::sysenv::output();
}
