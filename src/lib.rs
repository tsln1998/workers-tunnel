use crate::proxy::{parse_early_data, run_tunnel};
use crate::websocket::WebSocketStream;
use worker::*;

#[event(fetch)]
async fn main(req: Request, env: Env, _: Context) -> Result<Response> {
    // get user id
    let user_id = env.var("USER_ID")?.to_string();

    // ready early data
    let swp = req.headers().get("sec-websocket-protocol")?;
    let early_data = parse_early_data(swp)?;

    // Accept / handle a websocket connection
    let pair = WebSocketPair::new()?;
    let server = pair.server;
    server.accept()?;

    wasm_bindgen_futures::spawn_local(async move {
        let event_stream = server.events().expect("could not open stream");

        let socket = WebSocketStream::new(&server, event_stream, early_data);

        // run vless tunnel
        if let Err(err) = run_tunnel(socket, &user_id).await {
            if err.kind() == std::io::ErrorKind::InvalidData
                || err.kind() == std::io::ErrorKind::ConnectionAborted
            {
                server
                    .close(Some(1003), Some("invalid request"))
                    .unwrap_or_default()
            }
            console_debug!("run tunnel error: {}", err);
        }
    });

    Response::from_websocket(pair.client)
}

mod proxy {
    use std::io::{Error, ErrorKind, Result};
    use std::net::{Ipv4Addr, Ipv6Addr};

    use crate::websocket::WebSocketStream;
    use base64::{decode_config, URL_SAFE_NO_PAD};
    use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
    use worker::{console_debug, Socket};

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

    pub async fn run_tunnel(mut client_socket: WebSocketStream<'_>, user_id: &str) -> Result<()> {
        // process request

        // read version
        let mut prefix = [0u8; 18];
        client_socket.read_exact(&mut prefix).await?;

        if prefix[0] != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "invalid client protocol version, expected 0, got {}",
                    prefix[0]
                ),
            ));
        }

        // valid user id
        let target_id = &prefix[1..17];
        for (b1, b2) in parse_hex(user_id).iter().zip(target_id.iter()) {
            if b1 != b2 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid user id",
                ));
            }
        }

        {
            // ignore addons
            let addon_length = prefix[17];
            let mut addon_bytes = vec![0; addon_length as usize].into_boxed_slice();
            client_socket.read_exact(&mut addon_bytes).await?;
        }

        // parse remote address
        let mut address_prefix = [0u8; 4];
        client_socket.read_exact(&mut address_prefix).await?;

        match address_prefix[0] {
            1 => {
                // tcp, noop.
            }
            2 => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "UDP was requested",
                ));
            }
            unknown_protocol_type => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid requested protocol: {}", unknown_protocol_type),
                ));
            }
        }

        let port = ((address_prefix[1] as u16) << 8) | (address_prefix[2] as u16);

        let remote_addr = match address_prefix[3] {
            1 => {
                // 4 byte ipv4 address
                let mut address_bytes = [0u8; 4];
                client_socket.read_exact(&mut address_bytes).await?;

                let v4addr: Ipv4Addr = Ipv4Addr::new(
                    address_bytes[0],
                    address_bytes[1],
                    address_bytes[2],
                    address_bytes[3],
                );
                v4addr.to_string()
            }
            2 => {
                // domain name
                let mut domain_name_len = [0u8; 1];
                client_socket.read_exact(&mut domain_name_len).await?;

                let mut domain_name_bytes = vec![0; domain_name_len[0] as usize];
                client_socket.read_exact(&mut domain_name_bytes).await?;

                let address_str = match std::str::from_utf8(&domain_name_bytes) {
                    Ok(s) => s,
                    Err(e) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("invalid address: {}", e),
                        ));
                    }
                };
                address_str.to_string()
            }
            3 => {
                // 16 byte ipv6 address
                let mut address_bytes = [0u8; 16];
                client_socket.read_exact(&mut address_bytes).await?;

                let v6addr = Ipv6Addr::new(
                    ((address_bytes[0] as u16) << 8) | (address_bytes[1] as u16),
                    ((address_bytes[2] as u16) << 8) | (address_bytes[3] as u16),
                    ((address_bytes[4] as u16) << 8) | (address_bytes[5] as u16),
                    ((address_bytes[6] as u16) << 8) | (address_bytes[7] as u16),
                    ((address_bytes[8] as u16) << 8) | (address_bytes[9] as u16),
                    ((address_bytes[10] as u16) << 8) | (address_bytes[11] as u16),
                    ((address_bytes[12] as u16) << 8) | (address_bytes[13] as u16),
                    ((address_bytes[14] as u16) << 8) | (address_bytes[15] as u16),
                );
                format!("[{}]", v6addr)
            }
            invalid_type => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid address type: {}", invalid_type),
                ));
            }
        };

        // connect to remote socket
        let mut remote_socket = match Socket::builder().connect(remote_addr.clone(), port) {
            Ok(socket) => socket,
            Err(e) => {
                console_debug!(
                    "connect to remote {}:{} error: {}",
                    remote_addr,
                    port,
                    e.to_string()
                );

                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    e.to_string(),
                ));
            }
        };

        client_socket.write(&[0u8, 0u8]).await?;

        copy_bidirectional(&mut client_socket, &mut remote_socket).await?;

        Ok(())
    }

    fn parse_hex(hex_asm: &str) -> Box<[u8]> {
        let mut hex_bytes = hex_asm
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
        bytes.into_boxed_slice()
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
            let mut buff = BytesMut::new();
            if let Some(data) = early_data {
                buff.put_slice(&data)
            }

            Self {
                ws,
                stream,
                buffer: buff,
            }
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
            match self.ws.send_with_bytes(buf) {
                Ok(()) => Poll::Ready(Ok(buf.len())),
                Err(e) => Poll::Ready(Err(Error::new(ErrorKind::Other, e.to_string()))),
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
            match self.ws.close(None, Some("normal close")) {
                Ok(()) => Poll::Ready(Ok(())),
                Err(e) => Poll::Ready(Err(Error::new(ErrorKind::Other, e.to_string()))),
            }
        }
    }
}
