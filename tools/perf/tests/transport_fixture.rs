type ToolResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[cfg(target_os = "macos")]
#[path = "../../../src/native_trace_stdio.rs"]
mod native_trace_stdio;

mod transport {
    include!("../src/transport.rs");
    include!("support/transport_contract.rs");
}

fn main() -> ToolResult<()> {
    #[cfg(target_os = "macos")]
    native_trace_stdio::initialize()?;
    if let Ok(scenario) = std::env::var("BEND_PERF_TRANSPORT_FIXTURE") {
        transport::fixture_child(&scenario)
    } else {
        transport::contract_regressions()
    }
}
