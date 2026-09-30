use crate::support::Must;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use url::Url;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct LspClient {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<(Value, Instant)>,
    buffered: VecDeque<(Value, Instant)>,
    reader: Option<JoinHandle<()>>,
    next_id: i64,
}

impl LspClient {
    pub(crate) fn spawn(mut command: Command) -> Self {
        let (sender, messages) = mpsc::channel();
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = command.spawn().must_be("start Bend LSP");
        let stdin = child.stdin.take().must_be("LSP stdin");
        let stdout = child.stdout.take().must_be("LSP stdout");
        let reader = thread::spawn(move || read_messages(stdout, &sender));
        Self {
            child,
            stdin: Some(stdin),
            messages,
            buffered: VecDeque::new(),
            reader: Some(reader),
            next_id: 1,
        }
    }

    pub(crate) fn send(&mut self, message: &Value) {
        let body = serde_json::to_vec(message).must_be("serialize JSON-RPC message");
        let stdin = self.stdin.as_mut().must_be("open LSP stdin");
        write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).must_be("write LSP header");
        stdin.write_all(&body).must_be("write LSP body");
        stdin.flush().must_be("flush LSP message");
    }

    pub(crate) fn notify(&mut self, method: &str, params: Value) {
        let mut message = json!({"jsonrpc":"2.0", "method":method});
        if !params.is_null() {
            message["params"] = params;
        }
        self.send(&message);
    }

    pub(crate) fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send_request(method, params);
        let response = self
            .receive_matching(REQUEST_TIMEOUT, |message| message["id"] == id)
            .unwrap_or_else(|| panic!("timed out waiting for {method} response"));
        assert!(
            response.get("error").is_none(),
            "LSP request failed: {response}"
        );
        response
    }
    pub(crate) fn send_request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({"jsonrpc":"2.0", "id":id, "method":method});
        if !params.is_null() {
            message["params"] = params;
        }
        self.send(&message);
        id
    }

    pub(crate) fn receive_matching_timed(
        &mut self,
        timeout: Duration,
        predicate: impl Fn(&Value) -> bool,
    ) -> Option<(Value, Instant)> {
        if let Some(index) = self
            .buffered
            .iter()
            .position(|(message, _)| predicate(message))
        {
            return self.buffered.remove(index);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let Ok((message, received)) = self.messages.recv_timeout(remaining) else {
                return None;
            };
            if predicate(&message) {
                return Some((message, received));
            }
            self.buffered.push_back((message, received));
        }
    }

    pub(crate) fn receive_matching(
        &mut self,
        timeout: Duration,
        predicate: impl Fn(&Value) -> bool,
    ) -> Option<Value> {
        self.receive_matching_timed(timeout, predicate)
            .map(|(message, _)| message)
    }

    pub(crate) fn initialize(&mut self, root: &Path) {
        let root_uri = Url::from_directory_path(root).must_be("workspace URI");
        let response = self.request(
            "initialize",
            json!({
                "processId":null,
                "rootUri":root_uri,
                "capabilities":{},
                "workspaceFolders":[{"uri":root_uri,"name":"test"}]
            }),
        );
        assert!(response["result"]["capabilities"].is_object());
        self.notify("initialized", json!({}));
    }

    pub(crate) fn close_stdin(&mut self) {
        self.stdin.take();
    }

    pub(crate) fn process_id(&self) -> u32 {
        self.child.id()
    }

    #[cfg(unix)]
    pub(crate) fn sigterm(&self) {
        let status = Command::new("/bin/kill")
            .arg("-TERM")
            .arg(self.process_id().to_string())
            .status()
            .must_be("send SIGTERM");
        assert!(status.success(), "kill exited with {status}");
    }

    pub(crate) fn try_wait(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().must_be("poll LSP process")
    }

    pub(crate) fn wait_timeout(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait() {
                self.close_stdin();
                if let Some(reader) = self.reader.take() {
                    reader.join().must_be("join LSP output reader");
                }
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub(crate) fn finish(mut self) {
        self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        self.close_stdin();
        let status = self
            .wait_timeout(REQUEST_TIMEOUT)
            .must_be("wait for LSP exit");
        assert!(status.success(), "LSP exited with {status}");
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.close_stdin();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn read_messages(stdout: impl Read, sender: &mpsc::Sender<(Value, Instant)>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut content_length = None;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
        let Some(content_length) = content_length else {
            return;
        };
        let mut body = vec![0; content_length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let Ok(message) = serde_json::from_slice(&body) else {
            return;
        };
        if sender.send((message, Instant::now())).is_err() {
            return;
        }
    }
}
