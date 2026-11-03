//! One WebSocket to Chrome's browser endpoint, carrying every session.
//!
//! CDP's "flatten" mode puts all attached targets (the page, its out-of-process
//! iframes, service workers) on this single socket, told apart by `sessionId`.
//! Requests are matched to responses by `id`; everything without an `id` is an
//! event and goes to the handler given at connect time.

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// A CDP event as it came off the socket.
#[derive(Debug, Clone)]
pub struct Event {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>;

pub struct Conn {
    out: mpsc::UnboundedSender<Message>,
    next_id: AtomicU64,
    pending: Pending,
    closed: AtomicBool,
}

impl Conn {
    /// Connect and start the reader/writer tasks. `on_event` runs on the reader
    /// task, so it must not block — anything that needs another CDP round trip
    /// has to be spawned. `on_close` runs once, when the socket ends.
    pub async fn connect(
        ws_url: &str,
        on_event: impl Fn(Event) + Send + Sync + 'static,
        on_close: impl FnOnce() + Send + 'static,
    ) -> Result<Arc<Conn>, String> {
        // No size limits. tungstenite's defaults (16 MiB per frame, 64 MiB per
        // message) turn one large CDP answer — a big `get-html`, a response
        // body, a high-DPI screenshot — into a read error, and a read error
        // ends the connection, which ends the daemon. The peer is a local
        // Chrome we launched; there is no one to defend against here.
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(None)
            .max_frame_size(None);
        let (ws, _) = tokio_tungstenite::connect_async_with_config(ws_url, Some(config), true)
            .await
            .map_err(|e| format!("cannot open the DevTools WebSocket {ws_url}: {e}"))?;
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        let conn = Arc::new(Conn {
            out: tx,
            next_id: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        });

        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
        });

        let reader = conn.clone();
        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(Message::Text(t)) => reader.dispatch(t.as_str(), &on_event),
                    Ok(Message::Binary(b)) => {
                        if let Ok(t) = std::str::from_utf8(&b) {
                            reader.dispatch(t, &on_event);
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
            reader.closed.store(true, Ordering::SeqCst);
            // Fail everything in flight instead of letting callers wait out
            // their own timeouts on a socket that is already gone.
            if let Ok(mut p) = reader.pending.lock() {
                for (_, tx) in p.drain() {
                    let _ = tx.send(Err("CHROME_GONE: the DevTools connection closed".into()));
                }
            }
            on_close();
        });

        Ok(conn)
    }

    fn dispatch(&self, text: &str, on_event: &(impl Fn(Event) + Send + Sync)) {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return;
        };
        if let Some(id) = v.get("id").and_then(|x| x.as_u64()) {
            let tx = self.pending.lock().ok().and_then(|mut p| p.remove(&id));
            if let Some(tx) = tx {
                let r = match v.get("error") {
                    Some(err) => Err(format!("CDP error: {err}")),
                    None => Ok(v.get("result").cloned().unwrap_or_else(|| json!({}))),
                };
                let _ = tx.send(r);
            }
        } else if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
            on_event(Event {
                method: method.to_string(),
                params: v.get("params").cloned().unwrap_or(Value::Null),
                session_id: v
                    .get("sessionId")
                    .and_then(|s| s.as_str())
                    .map(String::from),
            });
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Send one command and wait for its response. `session` = `None` is the
    /// browser target itself.
    pub async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        if self.is_closed() {
            return Err("CHROME_GONE: the DevTools connection is closed".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let (tx, rx) = oneshot::channel();
        if let Ok(mut p) = self.pending.lock() {
            p.insert(id, tx);
        }
        if self
            .out
            .send(Message::Text(msg.to_string().into()))
            .is_err()
        {
            if let Ok(mut p) = self.pending.lock() {
                p.remove(&id);
            }
            return Err("CHROME_GONE: the DevTools connection is closed".into());
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("CHROME_GONE: the DevTools connection closed".into()),
            Err(_) => {
                if let Ok(mut p) = self.pending.lock() {
                    p.remove(&id);
                }
                Err(format!(
                    "CDP response timed out ({}ms) for {method} — the renderer may be busy",
                    timeout.as_millis()
                ))
            }
        }
    }

    /// Send without waiting for the answer (`Runtime.runIfWaitingForDebugger`
    /// from inside the event handler, where awaiting is not possible).
    pub fn send_nowait(&self, session: Option<&str>, method: &str, params: Value) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let _ = self.out.send(Message::Text(msg.to_string().into()));
    }
}
