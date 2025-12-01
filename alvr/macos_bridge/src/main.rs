//! ALVR macOS Bridge
//!
//! Native macOS process that receives frames from Wine via shared memory,
//! encodes with VideoToolbox, and streams to VR clients via ALVR protocol.
//!
//! Architecture:
//!   Wine (SteamVR/ALVR) --shared memory--> macOS Bridge --network--> AVP Client

mod encoder;
mod shared_memory;

use anyhow::{Context, Result};
use encoder::HevcEncoder;
use shared_memory::{FrameHeader, SharedMemory};
use std::time::{Duration, Instant};

use alvr_server_core::{ServerCoreContext, ServerCoreEvent};
use alvr_session::CodecType;

/// Default encoding settings
const DEFAULT_BITRATE_BPS: u32 = 30_000_000; // 30 Mbps
const DEFAULT_FPS: u32 = 72;

fn run_bridge() -> Result<()> {
    log::info!("ALVR macOS Bridge starting...");

    // Initialize ALVR filesystem layout
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("alvr");
    let log_dir = dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("alvr")
        .join("logs");

    std::fs::create_dir_all(&config_dir).ok();
    std::fs::create_dir_all(&log_dir).ok();

    alvr_server_core::initialize_environment(alvr_filesystem::Layout {
        config_dir: config_dir.clone(),
        log_dir: log_dir.clone(),
        ..Default::default()
    });

    // Create shared memory (this creates the file that Wine will map)
    let mut shm = SharedMemory::create().context("Failed to create shared memory")?;

    log::info!("Waiting for Wine to connect and set configuration...");

    // Wait for Wine to connect and set configuration
    let start = Instant::now();
    let timeout = Duration::from_secs(120);
    while !shm.is_configured() {
        if start.elapsed() > timeout {
            anyhow::bail!("Timeout waiting for Wine connection");
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let (width, height, format) = shm.get_config().unwrap();
    log::info!(
        "Wine connected! Resolution: {}x{}, format: 0x{:x}",
        width,
        height,
        format
    );

    // Create HEVC encoder
    let mut encoder =
        HevcEncoder::new(width, height, DEFAULT_BITRATE_BPS, DEFAULT_FPS).context("Failed to create encoder")?;

    // Initialize ALVR server core
    log::info!("Initializing ALVR server core...");
    let (server_context, event_receiver) = ServerCoreContext::new();
    server_context.start_connection();

    log::info!("ALVR server started. Waiting for client connection...");

    // Wait for client connection
    let mut client_connected = false;
    let connect_timeout = Duration::from_secs(60);
    let connect_start = Instant::now();

    while !client_connected && connect_start.elapsed() < connect_timeout {
        // Check for shutdown from Wine
        if shm.header().shutdown != 0 {
            log::info!("Shutdown signal from Wine before client connected");
            return Ok(());
        }

        // Poll for events
        if let Ok(event) = event_receiver.recv_timeout(Duration::from_millis(100)) {
            match event {
                ServerCoreEvent::ClientConnected => {
                    log::info!("Client connected!");
                    client_connected = true;
                }
                ServerCoreEvent::RequestIDR => {
                    log::debug!("IDR request (no client yet)");
                }
                _ => {}
            }
        }
    }

    if !client_connected {
        log::warn!("No client connected within timeout, continuing anyway...");
    }

    log::info!("Starting frame processing loop...");

    let mut frames_processed = 0u64;
    let mut frames_dropped_by_wine = 0u64;
    let mut force_idr = true; // Force first frame to be IDR

    loop {
        // Check for shutdown
        if shm.header().shutdown != 0 {
            log::info!("Shutdown signal received from Wine");
            break;
        }

        // Poll for server events
        while let Ok(event) = event_receiver.try_recv() {
            match event {
                ServerCoreEvent::ClientConnected => {
                    log::info!("Client connected!");
                    client_connected = true;
                    force_idr = true; // New client needs IDR
                }
                ServerCoreEvent::ClientDisconnected => {
                    log::info!("Client disconnected");
                    client_connected = false;
                }
                ServerCoreEvent::RequestIDR => {
                    log::debug!("IDR requested");
                    force_idr = true;
                }
                _ => {}
            }
        }

        // Try to acquire a frame from shared memory
        if let Some((buffer_idx, header, pixel_data)) = shm.try_acquire_frame() {
            // Encode the frame
            match encoder.encode_frame(pixel_data, force_idr || header.is_idr != 0) {
                Ok(Some(output)) => {
                    // Send config NALs if this is a keyframe and config not yet sent
                    if output.is_keyframe {
                        if let Some(config_nals) = &output.config_nals {
                            if !encoder.config_sent() {
                                log::info!("Sending codec config ({} bytes)", config_nals.len());
                                server_context
                                    .set_video_config_nals(config_nals.clone(), CodecType::Hevc);
                                encoder.mark_config_sent();
                            }
                        }
                    }

                    // Send the encoded NAL data
                    if client_connected {
                        let timestamp = Duration::from_nanos(header.timestamp_ns);
                        server_context.send_video_nal(timestamp, output.nal_data, output.is_keyframe);
                    }

                    force_idr = false;
                }
                Ok(None) => {
                    // Encoder didn't produce output yet (normal for pipelining)
                }
                Err(e) => {
                    log::error!("Encoding error: {:#}", e);
                }
            }

            // Release the buffer back to Wine
            shm.release_frame(buffer_idx);
            frames_processed += 1;

            // Log progress periodically
            if frames_processed % 300 == 0 {
                let stats = shm.header();
                log::info!(
                    "Processed {} frames (Wine: w={} e={} d={})",
                    frames_processed,
                    stats.frames_written,
                    stats.frames_encoded,
                    stats.frames_dropped
                );
            }
        } else {
            // No frame ready, sleep briefly to avoid busy-waiting
            std::thread::sleep(Duration::from_micros(500));
        }

        // Check for dropped frames by Wine
        let new_dropped = shm.header().frames_dropped;
        if new_dropped > frames_dropped_by_wine {
            log::warn!(
                "Wine dropped {} frames (encoder too slow?)",
                new_dropped - frames_dropped_by_wine
            );
            frames_dropped_by_wine = new_dropped;
        }
    }

    // Flush encoder
    log::info!("Flushing encoder...");
    if let Ok(outputs) = encoder.flush() {
        for output in outputs {
            if client_connected {
                server_context.send_video_nal(Duration::ZERO, output.nal_data, output.is_keyframe);
            }
        }
    }

    log::info!(
        "Bridge shutting down. Processed {} frames, Wine dropped {}",
        frames_processed,
        frames_dropped_by_wine
    );

    Ok(())
}

fn main() {
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    if let Err(e) = run_bridge() {
        log::error!("Bridge error: {:#}", e);
        std::process::exit(1);
    }
}
