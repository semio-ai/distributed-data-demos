mod zenoh;

use anyhow::Result;
use clap::Parser;

use variant_base::cli::CliArgs;
use variant_base::driver::run_protocol;

use crate::zenoh::ZenohVariant;

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = CliArgs::parse();
    let mut variant = ZenohVariant::new(&args.runner, &args.extra)?;
    run_protocol(&mut variant, &args)?;
    Ok(())
}
