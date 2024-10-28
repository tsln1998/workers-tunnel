use crate::proxy::{parse_early_data, parse_user_id, run_tunnel};
use crate::websocket::WebSocketStream;
use worker::*;

#[event(fetch)]
async fn main(req: Request, env: Env, _: Context) -> Result<Response> {
    // get user id
    let user_id = env.var("USER_ID")?.to_string();

    // ready early data
    let early_data = req.headers().get("sec-websocket-protocol")?;
    let early_data = parse_early_data(early_data)?;

    // Accept / handle a websocket connection
    let WebSocketPair { client, server } = WebSocketPair::new()?;
    server.accept()?;

    wasm_bindgen_futures::spawn_local(async move {
        let event_stream = server.events().expect("could not open stream");

        let user_id = parse_user_id(&user_id);
        let socket = WebSocketStream::new(&server, event_stream, early_data);

        // run vless tunnel
        if let Err(err) = run_tunnel(socket, &user_id).await {
            // log error
            console_error!("error: {}", err);

            // close websocket connection
            server
                .close(Some(1003), Some("invalid request"))
                .unwrap_or_default();
        }
    });

    Response::from_websocket(client)
}

#[allow(dead_code)]
mod protocol {
    pub const VERSION: u8 = 0;
    pub const RESPONSE: [u8; 2] = [0u8; 2];
    pub const NETWORK_TYPE_TCP: u8 = 1;
    pub const NETWORK_TYPE_UDP: u8 = 2;
    pub const ADDRESS_TYPE_IPV4: u8 = 1;
    pub const ADDRESS_TYPE_DOMAIN: u8 = 2;
    pub const ADDRESS_TYPE_IPV6: u8 = 3;
}

mod proxy {
    use std::io::{Error, ErrorKind, Result};
    use std::net::{Ipv4Addr, Ipv6Addr};

    use crate::ext::StreamExt;
    use crate::protocol;
    use crate::websocket::WebSocketStream;
    use base64::{decode_config, URL_SAFE_NO_PAD};
    use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
    use worker::Socket;

    pub fn parse_early_data(data: Option<String>) -> Result<Option<Vec<u8>>> {
        if let Some(data) = data {
            if !data.is_empty() {
                let s = data.replace('+', "-").replace('/', "_").replace("=", "");
                match decode_config(s, URL_SAFE_NO_PAD) {
                    Ok(early_data) => return Ok(Some(early_data)),
                    Err(err) => return Err(Error::new(ErrorKind::Other, err.to_string())),
                }
            }
        }
        Ok(None)
    }

    pub fn parse_user_id(user_id: &str) -> Vec<u8> {
        let mut hex_bytes = user_id
            .as_bytes()
            .iter()
            .filter_map(|b| match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(b - b'a' + 10),
                b'A'..=b'F' => Some(b - b'A' + 10),
                _ => None,
            })
            .fuse();

        let mut bytes = Vec::new();
        while let (Some(h), Some(l)) = (hex_bytes.next(), hex_bytes.next()) {
            bytes.push(h << 4 | l)
        }
        bytes
    }

    pub async fn run_tunnel(mut client_socket: WebSocketStream<'_>, user_id: &[u8]) -> Result<()> {
        // read version
        if client_socket.read_u8().await? != protocol::VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid client protocol version, expected 0",
            ));
        }

        // verify user_id
        if client_socket.read_bytes_n(16).await? != user_id {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid user id",
            ));
        }

        // ignore addons
        if client_socket.read_bytes_var().await?.len() > 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unsupported addons",
            ));
        }

        // read network type
        if client_socket.read_u8().await? != protocol::NETWORK_TYPE_TCP {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid network type, expected TCP",
            ));
        }

        // read remote port
        let remote_port = client_socket.read_u16().await?;

        // read remote address
        let remote_addr = match client_socket.read_u8().await? {
            protocol::ADDRESS_TYPE_DOMAIN => client_socket.read_string_var().await?,
            protocol::ADDRESS_TYPE_IPV4 => {
                Ipv4Addr::from_bits(client_socket.read_u32().await?).to_string()
            }
            protocol::ADDRESS_TYPE_IPV6 => format!(
                "[{}]",
                Ipv6Addr::from_bits(client_socket.read_u128().await?)
            ),
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid address type",
                ));
            }
        };

        // connect to remote socket
        let mut remote_socket = match Socket::builder().connect(remote_addr.clone(), remote_port) {
            Ok(socket) => socket,
            Err(e) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    format!(
                        "connect to remote {}:{} failed: {}",
                        remote_addr,
                        remote_port,
                        e.to_string()
                    ),
                ));
            }
        };

        // write response header
        client_socket.write(&protocol::RESPONSE).await?;

        // forward data
        copy_bidirectional(&mut client_socket, &mut remote_socket)
            .await
            .map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    format!(
                        "forward remote {}:{} failed: {}",
                        remote_addr,
                        remote_port,
                        e.to_string()
                    ),
                )
            })?;

        Ok(())
    }
}

mod ext {
    use std::io::Result;
    use tokio::io::AsyncReadExt;
    pub trait StreamExt {
        async fn read_string_var(&mut self) -> Result<String>;
        async fn read_string_n(&mut self, n: usize) -> Result<String>;
        async fn read_bytes_var(&mut self) -> Result<Vec<u8>>;
        async fn read_bytes_n(&mut self, n: usize) -> Result<Vec<u8>>;
    }

    impl<T: AsyncReadExt + Unpin + ?Sized> StreamExt for T {
        async fn read_string_var(&mut self) -> Result<String> {
            let length = self.read_u8().await?;
            self.read_string_n(length as usize).await
        }

        async fn read_string_n(&mut self, n: usize) -> Result<String> {
            self.read_bytes_n(n).await.map(|bytes| {
                String::from_utf8(bytes).map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid string: {}", e),
                    )
                })
            })?
        }

        async fn read_bytes_var(&mut self) -> Result<Vec<u8>> {
            let length = self.read_u8().await?;
            self.read_bytes_n(length as usize).await
        }

        async fn read_bytes_n(&mut self, n: usize) -> Result<Vec<u8>> {
            let mut buffer = vec![0u8; n];
            self.read_exact(&mut buffer).await?;

            Ok(buffer)
        }
    }
}

mod websocket {
    use futures_util::Stream;
    use std::{
        io::{Error, ErrorKind, Result},
        pin::Pin,
        task::{Context, Poll},
    };

    use bytes::{BufMut, BytesMut};
    use pin_project::pin_project;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use worker::{EventStream, WebSocket, WebsocketEvent};

    #[pin_project]
    pub struct WebSocketStream<'a> {
        ws: &'a WebSocket,
        #[pin]
        stream: EventStream<'a>,
        buffer: BytesMut,
    }

    impl<'a> WebSocketStream<'a> {
        pub fn new(
            ws: &'a WebSocket,
            stream: EventStream<'a>,
            early_data: Option<Vec<u8>>,
        ) -> Self {
            let mut buffer = BytesMut::new();
            if let Some(data) = early_data {
                buffer.put_slice(&data)
            }

            Self { ws, stream, buffer }
        }
    }

    impl<'a> AsyncRead for WebSocketStream<'a> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<Result<()>> {
            let mut this = self.project();

            loop {
                let amt = std::cmp::min(this.buffer.len(), buf.remaining());
                if amt > 0 {
                    buf.put_slice(&this.buffer.split_to(amt));
                    return Poll::Ready(Ok(()));
                }

                match this.stream.as_mut().poll_next(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Some(Ok(WebsocketEvent::Message(msg)))) => {
                        if let Some(data) = msg.bytes() {
                            this.buffer.put_slice(&data);
                        };
                        continue;
                    }
                    Poll::Ready(Some(Err(e))) => {
                        return Poll::Ready(Err(Error::new(ErrorKind::Other, e.to_string())))
                    }
                    _ => return Poll::Ready(Ok(())), // None or Close event, return Ok to indicate stream end
                }
            }
        }
    }

    impl<'a> AsyncWrite for WebSocketStream<'a> {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize>> {
            if let Err(e) = self.ws.send_with_bytes(buf) {
                return Poll::Ready(Err(Error::new(ErrorKind::Other, e.to_string())));
            }

            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
            if let Err(e) = self.ws.close(None, Some("normal close")) {
                return Poll::Ready(Err(Error::new(ErrorKind::Other, e.to_string())));
            }

            Poll::Ready(Ok(()))
        }
    }
}
