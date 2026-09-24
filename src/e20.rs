//! TCP client for the Hi-Flying EPORT-E20 serial device server, streaming
//! raw DSMR/e-MUCS P1 telegrams. Read-only; the E20 accepts up to 5
//! concurrent clients.

use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::dsmr::{parse_telegram, Telegram};
use crate::modbus_client::resolve_socket_addr;

/// Streams telegrams to `tx`, reconnecting with exponential backoff on
/// errors. Returns only once `tx` is closed.
pub async fn run(
    host: String,
    port: u16,
    tx: mpsc::Sender<Telegram>,
    backoff_min: Duration,
    backoff_max: Duration,
) {
    let mut backoff = backoff_min;
    loop {
        match connect_and_stream(&host, port, &tx, &mut backoff, backoff_min).await {
            Ok(()) => return,
            Err(e) => {
                warn!(error = %e, host = %host, port, backoff_ms = backoff.as_millis(), "E20 connection lost, reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(backoff_max);
            }
        }
    }
}

async fn connect_and_stream(
    host: &str,
    port: u16,
    tx: &mpsc::Sender<Telegram>,
    backoff: &mut Duration,
    backoff_min: Duration,
) -> std::io::Result<()> {
    let addr = resolve_socket_addr(host, port)
        .await
        .map_err(std::io::Error::other)?;
    let stream = TcpStream::connect(addr).await?;
    *backoff = backoff_min;
    info!(host, port, "connected to E20");
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    let mut in_telegram = false;

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "E20 closed connection",
            ));
        }

        if line.starts_with('/') {
            buf.clear();
            buf.push_str(&line);
            in_telegram = true;
            continue;
        }

        if !in_telegram {
            continue;
        }

        buf.push_str(&line);

        if line.starts_with('!') {
            in_telegram = false;
            match parse_telegram(&buf) {
                Ok(telegram) => {
                    if tx.send(telegram).await.is_err() {
                        return Ok(());
                    }
                }
                Err(e) => {
                    warn!(error = %e, "dropping telegram: parse/CRC failure");
                }
            }
            buf.clear();
        }
    }
}
