mod calibration;
mod common;
mod dashboard;
mod doctor;
mod hosted;
mod latency;
mod native;
mod platform_memory;
mod policy;
mod profiling;
mod reports;
mod scenario;
mod transport;
mod workflow;

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
        return Err(
            "Usage: cargo perf <doctor|compare|profile|open|dashboard|policy|calibration|reports|native> [arguments]"
                .into(),
        );
    };
    match command.as_str() {
        "doctor" => doctor::run(args),
        "compare" | "profile" => hosted::run(command, args),
        "open" => hosted::open_args(args),
        "dashboard" => dashboard::run(args),
        "collect-profile" => profiling::run(args),
        "collect-scenario" => scenario::run(args),
        "workflow" => workflow::run(args),
        "policy" => policy::run(args),
        "calibration" => calibration::run(args),
        "reports" => reports::run(args),
        "native" => native::run(args),
        "--help" | "-h" => {
            println!(
                "cargo perf <doctor|compare|profile|open> [arguments]\n\n\
                 doctor                         Check hosted-run prerequisites without measuring\n\
                 compare --base main --candidate HEAD\n\
                                                Collect paired native latency and process memory\n\
                 profile discovery-10000 --cpu  Collect a separate hosted CPU profile\n\
                 profile discovery-10000 --heap Collect a separate hosted allocation profile\n\
                 profile discovery-10000 --native\n\
                                                Collect the native OS diagnostic trace\n\
                 open <run-id|report-directory> Download or open a validated unified report\n\n\
                 Internal CI commands: collect-profile, collect-scenario, native, dashboard, workflow, reports, policy, calibration.\n\
                 All measurements require hosted CI. Doctor and report validation are safe locally."
            );
            Ok(())
        }
        _ => Err(format!("Unknown performance command: {command}").into()),
    }
}
