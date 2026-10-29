use crate::ToolResult;
use serde::Serialize;
use serde_json::{Value, json};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Serialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
enum Event {
    WriteAcknowledged {
        correlation: u64,
        request_id: Option<u64>,
        frame_started_ns: u128,
        acknowledgment_observed_ns: u128,
    },
    Response {
        request_id: u64,
        request_started_ns: u128,
        frame_received_ns: u128,
        response_observed_ns: u128,
    },
}

pub(super) struct ClientTrace {
    origin: Instant,
    origin_unix_upper_ns: u128,
    origin_uncertainty_ns: u128,
    correlation: u64,
    events: Vec<Event>,
}

impl ClientTrace {
    pub(super) fn new() -> ToolResult<Self> {
        let origin = Instant::now();
        let origin_unix_upper_ns = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let origin_uncertainty_ns = origin.elapsed().as_nanos();
        Ok(Self {
            origin,
            origin_unix_upper_ns,
            origin_uncertainty_ns,
            correlation: 0,
            events: Vec::with_capacity(1024),
        })
    }

    fn elapsed(&self, instant: Instant) -> ToolResult<u128> {
        Ok(instant
            .checked_duration_since(self.origin)
            .ok_or("Client I/O timestamp preceded its trace origin")?
            .as_nanos())
    }

    pub(super) fn sent(&mut self, request_id: Option<u64>, started: Instant) -> ToolResult<()> {
        let acknowledged = Instant::now();
        self.correlation = self
            .correlation
            .checked_add(1)
            .ok_or("Client I/O correlation exhausted")?;
        self.events.push(Event::WriteAcknowledged {
            correlation: self.correlation,
            request_id,
            frame_started_ns: self.elapsed(started)?,
            acknowledgment_observed_ns: self.elapsed(acknowledged)?,
        });
        Ok(())
    }

    pub(super) fn received(
        &mut self,
        request_id: u64,
        started: Instant,
        received: Instant,
    ) -> ToolResult<()> {
        let observed = Instant::now();
        self.events.push(Event::Response {
            request_id,
            request_started_ns: self.elapsed(started)?,
            frame_received_ns: self.elapsed(received)?,
            response_observed_ns: self.elapsed(observed)?,
        });
        Ok(())
    }

    pub(super) fn snapshot(&self) -> Value {
        json!({
            "format_version": 1,
            "origin_unix_upper_ns": self.origin_unix_upper_ns,
            "origin_uncertainty_ns": self.origin_uncertainty_ns,
            "units": "nanoseconds from the client monotonic origin",
            "write_acknowledgment": "caller observes completion after writer write_all+flush; not an exact syscall completion timestamp",
            "frame_received": "existing reader timestamp immediately after read_exact, before JSON parsing or response queue consumption",
            "correlation": "successful client requests and notifications in wire order; server-request replies excluded",
            "events": self.events,
        })
    }
}
