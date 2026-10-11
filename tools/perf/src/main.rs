mod calibration;
mod common;
mod latency;
mod native;
mod policy;
mod reports;
mod transport;

type ToolResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            if let Some(error) = error.downcast_ref::<clap::Error>() {
                if let Err(print_error) = error.print() {
                    eprintln!("error: {print_error}");
                }
                return std::process::ExitCode::from(if error.use_stderr() { 2 } else { 0 });
            }
            if let Some(error) = error.downcast_ref::<policy::ExitError>() {
                if !error.to_string().is_empty() {
                    eprintln!("error: {error}");
                }
                return std::process::ExitCode::from(error.status);
            }
            eprintln!("error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> ToolResult<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((command, args)) = args.split_first() else {
        return Err("Usage: bend2-perf <policy|calibration|reports|native> [arguments]".into());
    };
    match command.as_str() {
        "policy" => policy::run(args),
        "calibration" => calibration::run(args),
        "reports" => reports::run(args),
        "native" => native::run(args),
        "--help" | "-h" => {
            println!(
                "bend2-perf <policy|calibration|reports|native> [arguments]\nAll measurements require hosted CI. Report validation is safe locally."
            );
            Ok(())
        }
        _ => Err(format!("Unknown performance command: {command}").into()),
    }
}
