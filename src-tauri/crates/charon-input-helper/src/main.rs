#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = charon_input_helper::run() {
        eprintln!("[charon-input-helper] {error}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("charon-input-helper is supported only on Linux");
    std::process::exit(1);
}
