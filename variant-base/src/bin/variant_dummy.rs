use anyhow::Result;
use clap::Parser;

use variant_base::cli::CliArgs;
use variant_base::driver::run_protocol;
use variant_base::dummy::VariantDummy;

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = CliArgs::parse();
    let mut dummy = VariantDummy::new(&args.runner);
    run_protocol(&mut dummy, &args)?;
    Ok(())
}
