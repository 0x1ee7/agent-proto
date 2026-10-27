//! Entry point of the `a2ad` supervisor binary.

fn main() -> std::process::ExitCode {
    a2ad::cli::run()
}
