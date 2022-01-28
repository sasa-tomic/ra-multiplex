//! # LSP Multiplexer
//! Some LSP clients are not very smart about spawning the servers, for example coc-rust-analyzer
//! in neovim will spawn a new rust-analyzer instance per neovim instance, unfortunately this
//! wastes a _lot_ of resources.
//!
//! LSP Multiplexer attempts to solve this problem by spawning a single rust-analyzer instance per
//! cargo workspace and routing the messages through TCP to multiple clients.
//!
//! ## Language server protocol
//!
//! Specification can be found at
//! <https://microsoft.github.io/language-server-protocol/specifications/specification-current/>.
//!
//! We're not interested in supporting or even parsing the whole protocol, we only want a subset
//! that will allow us to mupltiplex messages between multiple clients and a single server.
//!
//! LSP has several main message types:
//!
//! ### Request Message
//! Requests from client to server. Requests contain an `id` property which is either `integer` or
//! `string`.
//!
//! ### Response Message
//! Responses from server for client requests. Also contain an `id` property, but according to the
//! the specification it can also be null, it's unclear what we should do when it is null. We could
//! either send the response to all clients or drop it.
//!
//! ### Notification Message
//! Notifications must not receive a response, this doesn't really mean anything to us as we're
//! just relaying the messages. It sounds like it'd allow us to simply pass a notification from any
//! client to the server and to pass a server notification to all clients, however there are some
//! subtypes of notifications defined by the LSP where that could be confusing to the client or
//! server:
//! - Cancel notifications - contains an `id` property again, so we could multiplex this like any
//!   other request
//! - Progress notifications - contains a `token` property which could be used to identify the
//!   client but the specification also says it has nothing to do with the request IDs

use anyhow::{ensure, Context, Result};
use serde_json::{Map, Value};
use std::net::Ipv4Addr;
use std::process::Stdio;
use std::str;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::task;
use ra_multiplex::{ProtoInit, PORT};

async fn process_client(socket: TcpStream, port: u16) -> Result<()> {
    log::debug!("accepted {port}");

    let (socket_read, socket_write) = socket.into_split();
    let mut socket_read = BufReader::new(socket_read);

    let mut header = Vec::new();
    socket_read
        .read_until(b'\0', &mut header)
        .await
        .context("read proto init")?;
    header.pop();

    let proto_init: ProtoInit =
        serde_json::from_slice(&header).context("invalid proto init")?;
    ensure!(proto_init.check_version(), "invalid protocol version");

    let child = Command::new("rust-analyzer")
        .args(&proto_init.args)
        .current_dir(&proto_init.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("cannot spawn rust-analyzer")?;

    let child_stdin = child.stdin.unwrap();
    let child_stdout = BufReader::new(child.stdout.unwrap());

    task::spawn(async move { copy_io("recv", socket_read, child_stdin, port).await });
    task::spawn(async move { copy_io("send", child_stdout, socket_write, port).await });
    Ok(())
}

async fn copy_io<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    tag: &'static str,
    mut read: R,
    mut write: W,
    port: u16,
) -> Result<()> {
    let mut header = Vec::new();
    let mut packet = Vec::new();

    loop {
        let mut content_type = None;
        let mut content_len = None;

        loop {
            // read headers
            header.clear();
            read.read_until(b'\n', &mut header)
                .await
                .context("read header")?;
            let header_text = header
                .strip_suffix(b"\r\n")
                .expect("malformed header, missing \\r\\n");

            if header_text.is_empty() {
                // header is separated by nothing
                break;
            }
            if let Some(value) = header_text.strip_prefix(b"Content-Type: ") {
                content_type = Some(value.to_owned());
                continue;
            }
            if let Some(value) = header_text.strip_prefix(b"Content-Length: ") {
                content_len = Some(
                    str::from_utf8(value)
                        .expect("invalid utf8")
                        .parse::<usize>()
                        .expect("invalid content length"),
                );
                continue;
            }
            panic!("invalid header: {}", String::from_utf8_lossy(header_text));
        }

        let _ = content_type; // ignore content-type if present
        let content_len = content_len.expect("missing content-length");

        packet.resize(content_len, 0);
        read.read_exact(&mut packet).await.context("read body")?;

        let json: Map<String, Value> = serde_json::from_slice(&packet).expect("invalid packet");
        if let Some(id) = json.get("id") {
            log::info!("{tag} port={port}, message_id={id:?}");
        }

        write
            .write_all(format!("Content-Length: {}\r\n\r\n", content_len).as_bytes())
            .await
            .context("write header")?;
        write.write_all(&packet).await.context("write packet")?;
        write.flush().await.context("flush socket")?;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();

    let listener = TcpListener::bind((Ipv4Addr::new(0, 0, 0, 0), PORT))
        .await
        .context("listen")?;

    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                task::spawn(async move {
                    if let Err(err) = process_client(socket, addr.port()).await {
                        log::error!("{err}");
                    }
                });
            }
            Err(err) => match err.kind() {
                // ignore benign errors
                std::io::ErrorKind::NotConnected => {}
                _ => {
                    Err(err).context("accept connection")?;
                }
            },
        }
    }
}