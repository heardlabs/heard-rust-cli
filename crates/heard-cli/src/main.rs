//! The `heard` binary. Everything lives in the `heard_cli` library.

fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(heard_cli::run())
}
