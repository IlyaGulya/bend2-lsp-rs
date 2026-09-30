use std::{
    collections::HashMap,
    io::{self, Read},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use super::{lsp::Backend, telemetry};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{Mutex as AsyncMutex, mpsc},
    task::JoinHandle,
};
use tower::Service;
use tower_lsp::{
    LspService, Server,
    jsonrpc::{Id, Request, Response},
};
use tracing::Instrument;
use url::Url;

static NEXT_REQUEST_CORRELATION: AtomicU64 = AtomicU64::new(1);

fn method_label(method: &str) -> &'static str {
    match method {
        "initialize" => "initialize",
        "shutdown" => "shutdown",
        "exit" => "exit",
        "$/cancelRequest" => "cancel",
        "textDocument/hover" => "hover",
        "textDocument/definition" => "definition",
        "textDocument/typeDefinition" => "type_definition",
        "textDocument/references" => "references",
        _ => "other",
    }
}

struct TracedStdout<W>(W);

impl<W: AsyncWrite + Unpin> AsyncWrite for TracedStdout<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        tracing::info_span!("transport.stdout_write", bytes = bytes.len())
            .in_scope(|| Pin::new(&mut self.0).poll_write(context, bytes))
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        tracing::info_span!("transport.stdout_flush")
            .in_scope(|| Pin::new(&mut self.0).poll_flush(context))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(context)
    }
}

struct ExitAwareService<S> {
    inner: S,
    exit: tokio::sync::watch::Sender<bool>,
}

impl<S> Service<Request> for ExitAwareService<S>
where
    S: Service<Request, Response = Option<Response>> + Send + 'static,
{
    type Response = Option<Response>;
    type Error = S::Error;
    type Future = tracing::instrument::Instrumented<S::Future>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        if request.method() == "exit" {
            self.exit.send_replace(true);
        }
        let (call_span, future_span) = if tracing::enabled!(tracing::Level::INFO) {
            let correlation = NEXT_REQUEST_CORRELATION.fetch_add(1, Ordering::Relaxed);
            let method = method_label(request.method());
            let call_span = tracing::info_span!(
                "transport.service_call",
                correlation,
                method,
                request_id = tracing::field::Empty
            );
            let future_span = tracing::info_span!(
                "transport.service_future",
                correlation,
                method,
                request_id = tracing::field::Empty
            );
            if let Some(Id::Number(id)) = request.id() {
                call_span.record("request_id", *id);
                future_span.record("request_id", *id);
            }
            (call_span, future_span)
        } else {
            (tracing::Span::none(), tracing::Span::none())
        };
        call_span
            .in_scope(|| self.inner.call(request))
            .instrument(future_span)
    }
}

struct StdinChannel {
    receiver: mpsc::Receiver<Vec<u8>>,
    current: Vec<u8>,
    offset: usize,
}

impl StdinChannel {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel(4);
        std::thread::spawn(move || {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();
            loop {
                let mut chunk = vec![0; 8192];
                match stdin.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        tracing::info!(bytes = read, "transport.stdin_read");
                        chunk.truncate(read);
                        if sender.blocking_send(chunk).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            receiver,
            current: Vec::new(),
            offset: 0,
        }
    }
}

impl AsyncRead for StdinChannel {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        loop {
            if this.offset < this.current.len() {
                let length = buffer.remaining().min(this.current.len() - this.offset);
                if length == 0 {
                    return Poll::Ready(Ok(()));
                }
                tracing::info!(bytes = length, "transport.stdin_chunk");
                buffer.put_slice(&this.current[this.offset..this.offset + length]);
                this.offset += length;
                if this.offset == this.current.len() {
                    this.current.clear();
                    this.offset = 0;
                }
                return Poll::Ready(Ok(()));
            }
            match this.receiver.poll_recv(context) {
                Poll::Ready(Some(chunk)) => {
                    this.current = chunk;
                    this.offset = 0;
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

pub(super) async fn run() {
    let trace_guard = match telemetry::initialize() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!(
                "bend2-lsp: tracing initialization failed ({})",
                error.kind()
            );
            None
        }
    };
    let stdout = tokio::io::stdout();
    let analysis_tasks = Arc::new(AsyncMutex::new(HashMap::new()));
    let server_tasks = analysis_tasks.clone();
    let (service, socket) =
        LspService::new(move |client| Backend::new(client, server_tasks.clone()));
    let (exit, mut exit_rx) = tokio::sync::watch::channel(false);
    let service = ExitAwareService {
        inner: service,
        exit,
    };
    tokio::select! {
        () = Server::new(StdinChannel::new(), TracedStdout(stdout), socket).serve(service) => {}
        _ = exit_rx.changed() => {}
        () = termination_signal() => {}
    }
    cancel_task_handles(&analysis_tasks).await;
    drop(trace_guard);
}

#[cfg(unix)]
async fn termination_signal() {
    let Ok(mut signals) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    else {
        eprintln!("bend2-lsp: unable to register SIGTERM handler");
        std::future::pending::<()>().await;
        return;
    };
    if signals.recv().await.is_none() {
        std::future::pending::<()>().await;
    }
}

#[cfg(not(unix))]
async fn termination_signal() {
    std::future::pending::<()>().await;
}

async fn cancel_task_handles(tasks: &AsyncMutex<HashMap<Url, (u64, JoinHandle<()>)>>) {
    let tasks = std::mem::take(&mut *tasks.lock().await);
    for (_, task) in tasks.into_values() {
        task.abort();
        let _ = task.await;
    }
}
