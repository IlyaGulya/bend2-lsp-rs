type ToolResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

mod transport {
    include!("../src/transport.rs");
    include!("support/transport_contract.rs");
}

fn main() -> ToolResult<()> {
    if let Ok(scenario) = std::env::var("BEND_PERF_TRANSPORT_FIXTURE") {
        transport::fixture_child(&scenario)
    } else {
        transport::contract_regressions()
    }
}
