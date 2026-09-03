use std::{env, io};

use anyhow::{Context, Result, bail};
use freemdu::{
    embedded_io_async::{ErrorType, Read, Write},
    serial,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

const BRIDGE_PREFIX: &str = "tcp://";
const DEFAULT_BRIDGE_TOKEN_ENV: &str = "FREEMDU_BRIDGE_TOKEN";
const AUTH_LINE_MAX: usize = 256;

pub enum Port {
    Serial(serial::Port),
    Tcp(TcpStream),
}

impl ErrorType for Port {
    type Error = io::Error;
}

impl Read for Port {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        match self {
            Self::Serial(port) => Read::read(port, buf)
                .await
                .map_err(|err| io::Error::other(err.to_string())),
            Self::Tcp(stream) => AsyncReadExt::read(stream, buf).await,
        }
    }
}

impl Write for Port {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        match self {
            Self::Serial(port) => Write::write(port, buf)
                .await
                .map_err(|err| io::Error::other(err.to_string())),
            Self::Tcp(stream) => AsyncWriteExt::write(stream, buf).await,
        }
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        match self {
            Self::Serial(port) => Write::flush(port)
                .await
                .map_err(|err| io::Error::other(err.to_string())),
            Self::Tcp(stream) => AsyncWriteExt::flush(stream).await,
        }
    }
}

pub async fn open(endpoint: &str, cli_token: Option<&str>) -> Result<Port> {
    let Some(address) = endpoint.strip_prefix(BRIDGE_PREFIX) else {
        return Ok(Port::Serial(
            serial::open(endpoint).context("Failed to open serial port")?,
        ));
    };

    if address.is_empty() {
        bail!("TCP bridge endpoint is empty");
    }

    let token = match cli_token {
        Some(token) if !token.is_empty() => token.to_owned(),
        _ => env::var(DEFAULT_BRIDGE_TOKEN_ENV).with_context(|| {
            format!(
                "TCP bridge requires --token or the {DEFAULT_BRIDGE_TOKEN_ENV} environment variable"
            )
        })?,
    };

    if token.chars().any(char::is_whitespace) {
        bail!("bridge token must not contain whitespace");
    }

    let mut stream = TcpStream::connect(address)
        .await
        .with_context(|| format!("Failed to connect to Wi-Fi bridge at {address}"))?;

    stream
        .write_all(format!("FMDUBRIDGE1 {token}\n").as_bytes())
        .await
        .context("Failed to authenticate with Wi-Fi bridge")?;

    let mut response = Vec::new();

    while response.len() < AUTH_LINE_MAX {
        let mut byte = [0_u8; 1];
        let len = stream
            .read(&mut byte)
            .await
            .context("Failed to read Wi-Fi bridge authentication response")?;

        if len == 0 {
            bail!("Wi-Fi bridge closed connection during authentication");
        }

        response.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }

    if !response.ends_with(b"\n") {
        bail!("Wi-Fi bridge authentication response is too long");
    }

    let response = std::str::from_utf8(&response)
        .context("Wi-Fi bridge returned an invalid authentication response")?
        .trim_end();

    if !response.starts_with("OK ") {
        bail!("Wi-Fi bridge rejected connection: {response}");
    }

    Ok(Port::Tcp(stream))
}
