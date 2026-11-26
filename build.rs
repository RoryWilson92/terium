/// Rebuild when `cfg.toml` changes, and pass the ESP-IDF environment through to rustc.
fn main() {
    println!("cargo:rerun-if-changed=cfg.toml");
    // esp-idf-sys sets one of these for the chip being built. `radio.rs` switches on them.
    println!("cargo:rustc-check-cfg=cfg(esp32)");
    println!("cargo:rustc-check-cfg=cfg(esp32c5)");
    // Include paths, linker script, and the ESP-IDF build flags.
    embuild::espidf::sysenv::output();
}
