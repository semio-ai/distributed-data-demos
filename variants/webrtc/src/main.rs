//! `variant-webrtc` binary entry point.
//!
//! Parses CLI arguments, derives signaling and media ports from the
//! `--peers` map, constructs a `WebRtcVariant`, and hands it to
//! `variant_base::driver::run_protocol`.

mod pairing;
mod protocol;
mod signaling;
mod webrtc;

use anyhow::{Context, Result};
use clap::Parser;

use variant_base::cli::{parse_extra_arg, CliArgs};
use variant_base::driver::run_protocol;

use crate::pairing::{derive_endpoints, parse_peers};
use crate::webrtc::WebRtcVariant;

fn main() {
    variant_base::print_build_banner!("webrtc");
    if let Err(e) = run() {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = CliArgs::parse();

    let signaling_base_port = parse_extra_arg(&args.extra, "signaling-base-port")
        .context("missing required --signaling-base-port in variant-specific args")?
        .parse::<u16>()
        .context("invalid --signaling-base-port (expected u16)")?;

    let media_base_port = parse_extra_arg(&args.extra, "media-base-port")
        .context("missing required --media-base-port in variant-specific args")?
        .parse::<u16>()
        .context("invalid --media-base-port (expected u16)")?;

    let peers_raw = parse_extra_arg(&args.extra, "peers")
        .context("missing runner-injected --peers argument")?;
    let peer_map = parse_peers(&peers_raw).context("failed to parse --peers")?;

    let derived = derive_endpoints(
        &peer_map,
        &args.runner,
        signaling_base_port,
        media_base_port,
        args.qos,
    )
    .context("port derivation failed")?;

    eprintln!(
        "[webrtc] runner={} qos={} signaling_listen={} media_listen={} peers={}",
        args.runner,
        args.qos,
        derived.signaling_listen,
        derived.media_listen,
        derived
            .peers
            .iter()
            .map(|p| format!(
                "{}->{}@{}({:?})",
                p.name, p.signaling_addr, p.media_addr, p.role
            ))
            .collect::<Vec<_>>()
            .join(",")
    );

    let mut variant = WebRtcVariant::new(
        &args.runner,
        derived.signaling_listen,
        derived.media_listen,
        derived.peers,
    );
    run_protocol(&mut variant, &args)?;
    Ok(())
}
