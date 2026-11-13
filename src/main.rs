use std::process::ExitCode;

fn main() -> ExitCode {
    match greenshift::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("greenshift: error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
