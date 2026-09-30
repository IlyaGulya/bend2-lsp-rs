use std::{fs::OpenOptions, io};

use tracing_chrome::{ChromeLayerBuilder, EventOrSpan, FlushGuard, TraceStyle};
use tracing_subscriber::{filter::filter_fn, prelude::*};

const TRACE_ENV: &str = "BEND2_LSP_TRACE";

pub(super) fn initialize() -> io::Result<Option<FlushGuard>> {
    let Some(path) = std::env::var_os(TRACE_ENV).filter(|path| !path.is_empty()) else {
        return Ok(None);
    };
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    let (chrome_layer, guard) = ChromeLayerBuilder::new()
        .writer(file)
        .include_args(true)
        .include_locations(false)
        .trace_style(TraceStyle::Async)
        .name_fn(Box::new(|event_or_span| match event_or_span {
            EventOrSpan::Event(_) => "event".to_owned(),
            EventOrSpan::Span(span) => span.metadata().name().to_owned(),
        }))
        .category_fn(Box::new(|_| "bend2-lsp".to_owned()))
        .build();
    let subscriber =
        tracing_subscriber::registry().with(chrome_layer.with_filter(filter_fn(|metadata| {
            metadata.target() == "bend2_lsp" || metadata.target().starts_with("bend2_lsp::")
        })));
    subscriber.try_init().map_err(io::Error::other)?;
    Ok(Some(guard))
}
