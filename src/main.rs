use std::process::ExitCode;

fn main() -> ExitCode {
    match greenshift::cli::run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("greenshift: error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
