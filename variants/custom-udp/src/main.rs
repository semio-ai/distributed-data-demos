mod protocol;
mod qos;
mod udp;

use anyhow::Result;
use clap::Parser;

use variant_base::cli::CliArgs;
use variant_base::driver::run_protocol;
use variant_base::Qos;

use udp::{UdpConfig, UdpVariant};

fn main() {
    if let Err(e) = run() {
        eprintln!("[custom-udp] error: {:#}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = CliArgs::parse();

    let qos = Qos::from_int(args.qos)
        .ok_or_else(|| anyhow::anyhow!("invalid QoS level: {}", args.qos))?;

    let config = UdpConfig::from_extra(&args.extra, &args.runner, qos)?;
    let mut variant = UdpVariant::new(config);

    run_protocol(&mut variant, &args)?;

    Ok(())
}
