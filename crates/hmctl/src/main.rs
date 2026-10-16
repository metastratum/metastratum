//! hypermon admin CLI.

use clap::Parser;

#[derive(Parser)]
#[command(version, about)]
struct Args {}

fn main() -> anyhow::Result<()> {
    let _args = Args::parse();
    Ok(())
}
