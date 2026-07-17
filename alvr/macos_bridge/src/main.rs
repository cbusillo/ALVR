#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    if std::env::var("ALVR_BRIDGE_INPUT").as_deref() == Ok("iosurface") {
        let config = alvr_macos_bridge::NativeSourceConfig::from_env()?;
        let summary =
            alvr_macos_bridge::run_native_source_probe(config, |report| println!("{report}"))?;
        println!("{summary}");
    } else {
        let config = alvr_macos_bridge::ProbeConfig::from_env()?;
        let summary = alvr_macos_bridge::run_surface_probe(config, |report| println!("{report}"))?;
        println!("{summary}");
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("alvr_macos_bridge is only supported on macOS");
}
