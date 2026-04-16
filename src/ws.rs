use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::io::ErrorKind;
use std::net::TcpStream;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Error as WsError, Message, WebSocket};
use url::Url;

pub(crate) struct WsJsonRpcTransport {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
}

impl WsJsonRpcTransport {
    pub(crate) fn connect(url: &str) -> Result<Self> {
        let url = Url::parse(url).with_context(|| format!("invalid websocket URL: {url}"))?;
        let (socket, _) = connect(url.as_str()).context("failed to connect websocket transport")?;
        Ok(Self { socket })
    }

    pub(crate) fn write_json(&mut self, value: &Value) -> Result<()> {
        self.socket
            .send(Message::Text(serde_json::to_string(value)?.into()))
            .context("failed to write websocket JSON-RPC message")?;
        Ok(())
    }

    pub(crate) fn read_json(&mut self) -> Result<Value> {
        loop {
            match self
                .socket
                .read()
                .context("failed to read websocket JSON-RPC message")?
            {
                Message::Text(text) => {
                    return serde_json::from_str(&text).with_context(|| {
                        format!("invalid JSON from websocket app-server: {text}")
                    });
                }
                Message::Binary(bytes) => {
                    return serde_json::from_slice(&bytes).with_context(|| {
                        format!(
                            "invalid binary JSON from websocket app-server ({} bytes)",
                            bytes.len()
                        )
                    });
                }
                Message::Ping(payload) => {
                    self.socket
                        .send(Message::Pong(payload))
                        .context("failed to answer websocket ping")?;
                }
                Message::Pong(_) | Message::Frame(_) => {}
                Message::Close(frame) => {
                    let reason = frame
                        .as_ref()
                        .map(|value| value.reason.to_string())
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| "no reason provided".to_string());
                    bail!("websocket app-server closed connection: {reason}");
                }
            }
        }
    }

    pub(crate) fn try_read_json(&mut self) -> Result<Option<Value>> {
        self.set_nonblocking(true)?;
        let read = match self.read_json() {
            Ok(message) => Ok(Some(message)),
            Err(error) if is_would_block(&error) => Ok(None),
            Err(error) => Err(error),
        };
        self.set_nonblocking(false)?;
        read
    }

    fn set_nonblocking(&mut self, value: bool) -> Result<()> {
        match self.socket.get_mut() {
            MaybeTlsStream::Plain(stream) => stream
                .set_nonblocking(value)
                .with_context(|| format!("failed to set websocket nonblocking={value}")),
            #[allow(unreachable_patterns)]
            _ => bail!("unsupported websocket stream type"),
        }
    }
}

fn is_would_block(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|source| source.downcast_ref::<WsError>())
        .any(|source| matches!(source, WsError::Io(io) if io.kind() == ErrorKind::WouldBlock))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn websocket_transport_reads_and_writes_json() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake ws server");
        let address = listener.local_addr().expect("fake ws server addr");
        let (requests_tx, requests_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept websocket client");
            let mut socket = tungstenite::accept(stream).expect("accept websocket");
            let request = socket.read().expect("read websocket request");
            let text = request.into_text().expect("text websocket request");
            requests_tx
                .send(serde_json::from_str::<Value>(&text).expect("parse websocket request"))
                .expect("send parsed request");
            socket
                .send(Message::Text(
                    serde_json::to_string(&json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": { "ok": true }
                    }))
                    .expect("serialize websocket response")
                    .into(),
                ))
                .expect("write websocket response");
        });

        let mut transport =
            WsJsonRpcTransport::connect(&format!("ws://{address}")).expect("connect transport");
        transport
            .write_json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {}
            }))
            .expect("write websocket JSON");
        assert_eq!(
            transport.read_json().expect("read websocket JSON"),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "ok": true }
            })
        );
        assert_eq!(
            requests_rx.recv().expect("captured websocket request"),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {}
            })
        );
        server.join().expect("fake ws server thread");
    }
}
