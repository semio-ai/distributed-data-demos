mod hybrid;
mod protocol;
mod tcp;
mod udp;

use anyhow::Result;
use clap::Parser;

use variant_base::cli::CliArgs;
use variant_base::driver::run_protocol;

use hybrid::{HybridConfig, HybridVariant};

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = CliArgs::parse();
    let config = HybridConfig::from_extra_args(&args.extra)?;
    let mut variant = HybridVariant::new(&args.runner, config);
    run_protocol(&mut variant, &args)?;
    Ok(())
}
