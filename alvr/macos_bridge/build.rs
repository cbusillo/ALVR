fn main() {
    use std::{env, path::PathBuf, process::Command};

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let output_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR must be set"));
    let air_path = output_dir.join("bgra_to_nv12.air");
    let metallib_path = output_dir.join("bgra_to_nv12.metallib");

    let metal_status = Command::new("xcrun")
        .args(["-sdk", "macosx", "metal", "-c"])
        .arg("src/bgra_to_nv12.metal")
        .arg("-o")
        .arg(&air_path)
        .status()
        .expect("failed to launch Metal compiler");
    assert!(metal_status.success(), "Metal shader compilation failed");

    let metallib_status = Command::new("xcrun")
        .args(["-sdk", "macosx", "metallib"])
        .arg(&air_path)
        .arg("-o")
        .arg(&metallib_path)
        .status()
        .expect("failed to launch metallib linker");
    assert!(metallib_status.success(), "Metal library linking failed");

    cc::Build::new()
        .cpp(true)
        .file("src/metal_converter.mm")
        .flag("-std=c++17")
        .flag("-fobjc-arc")
        .compile("alvr_macos_metal_converter");
    cc::Build::new()
        .file("src/native_source.c")
        .include("src")
        .flag("-std=c11")
        .compile("alvr_macos_native_source");

    println!("cargo:rustc-link-lib=framework=CoreFoundation");
    println!("cargo:rustc-link-lib=framework=CoreVideo");
    println!("cargo:rustc-link-lib=framework=IOSurface");
    println!("cargo:rustc-link-lib=framework=Metal");
    println!("cargo:rustc-link-lib=bsm");
    println!("cargo:rustc-link-lib=proc");
    println!("cargo:rerun-if-changed=src/bgra_to_nv12.metal");
    println!("cargo:rerun-if-changed=src/metal_converter.mm");
    println!("cargo:rerun-if-changed=src/native_source.c");
    println!("cargo:rerun-if-changed=src/iosurface_handoff_protocol.h");
}
