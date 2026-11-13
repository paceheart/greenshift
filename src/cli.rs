//! Command-line interface.

use clap::Parser;

/// Pick which tests to run from what your tests have done before.
#[derive(Debug, Parser)]
#[command(name = "greenshift", version, about, long_about = None)]
pub struct Cli {}

/// Parse `std::env::args` and run the requested command.
pub fn run() -> anyhow::Result<()> {
    let _cli = Cli::parse();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
