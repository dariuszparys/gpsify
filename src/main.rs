use std::process::ExitCode;

fn main() -> ExitCode {
    println!("gpsify {}", gpsify::VERSION);
    ExitCode::SUCCESS
}
