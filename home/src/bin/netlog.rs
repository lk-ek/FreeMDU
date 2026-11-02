use core::{
    fmt::Write as _,
    sync::atomic::{AtomicBool, Ordering},
};
use embassy_futures::select::{Either, select};
use embassy_net::{Stack, tcp::TcpSocket};
use embassy_sync::{
    blocking_mutex::{Mutex, raw::CriticalSectionRawMutex},
    channel::Channel,
};
use embassy_time::{Duration, Timer, with_timeout};
use embedded_io_async::Write as _;
use log::{Level, LevelFilter, Log, Metadata, Record};

const ESP_LOG: &str = env!("ESP_LOG");
const LOG_PORT: u16 = freemdu_home::num_from_env!("LOG_PORT", u16);
const OTA_TOKEN: &str = env!("OTA_TOKEN");
// Fixed memory, no allocation or network waits in the synchronous logger.
static LINES: Channel<CriticalSectionRawMutex, LogLine, 16> = Channel::new();
static DROPPED: Mutex<CriticalSectionRawMutex, core::cell::Cell<u32>> =
    Mutex::new(core::cell::Cell::new(0));
static LOGGER: SerialLogger = SerialLogger;
static QUIET_DIAGNOSTIC_SCAN: AtomicBool = AtomicBool::new(false);

struct SerialLogger;

impl Log for SerialLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        enabled_for(metadata.level(), metadata.target())
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        // Brute-force diagnostics perform thousands of optical transactions.
        // Suppress Debug/Trace formatting and USB output while a scan is active.
        if QUIET_DIAGNOSTIC_SCAN.load(Ordering::Relaxed) && record.level() > Level::Info {
            return;
        }

        esp_println::println!("{} - {}", record.level(), record.args());
        let mut line = LogLine::new();
        let _ = write!(&mut line, "{} - {}", record.level(), record.args());
        line.finish();
        if let Err(embassy_sync::channel::TrySendError::Full(line)) = LINES.try_send(line) {
            // Retain recent records, including when nobody is connected.
            let _ = LINES.try_receive();
            DROPPED.lock(|count| count.set(count.get().saturating_add(1)));
            if LINES.try_send(line).is_err() {
                DROPPED.lock(|count| count.set(count.get().saturating_add(1)));
            }
        }
    }

    fn flush(&self) {}
}

pub fn set_quiet_diagnostic_scan(quiet: bool) {
    QUIET_DIAGNOSTIC_SCAN.store(quiet, Ordering::Relaxed);
}

pub fn init() {
    unsafe {
        log::set_logger_racy(&LOGGER).expect("logger already initialized");

        // Filtering is performed in `enabled_for`. Leave the global ceiling
        // at Trace so target-specific ESP_LOG directives can select any level.
        log::set_max_level_racy(LevelFilter::Trace);
    }
}

fn enabled_for(level: Level, target: &str) -> bool {
    let mut selected = LevelFilter::Info;
    let mut selected_prefix_len = 0_usize;

    for directive in ESP_LOG.split(',').filter(|item| !item.is_empty()) {
        if let Some((prefix, filter)) = directive.split_once('=') {
            if target.starts_with(prefix)
                && prefix.len() >= selected_prefix_len
                && let Some(filter) = parse_filter(filter)
            {
                selected = filter;
                selected_prefix_len = prefix.len();
            }
        } else if let Some(filter) = parse_filter(directive) {
            selected = filter;
        }
    }

    match selected {
        LevelFilter::Off => false,
        LevelFilter::Error => level <= Level::Error,
        LevelFilter::Warn => level <= Level::Warn,
        LevelFilter::Info => level <= Level::Info,
        LevelFilter::Debug => level <= Level::Debug,
        LevelFilter::Trace => true,
    }
}

fn parse_filter(value: &str) -> Option<LevelFilter> {
    if value.eq_ignore_ascii_case("off") {
        Some(LevelFilter::Off)
    } else if value.eq_ignore_ascii_case("error") {
        Some(LevelFilter::Error)
    } else if value.eq_ignore_ascii_case("warn") {
        Some(LevelFilter::Warn)
    } else if value.eq_ignore_ascii_case("info") {
        Some(LevelFilter::Info)
    } else if value.eq_ignore_ascii_case("debug") {
        Some(LevelFilter::Debug)
    } else if value.eq_ignore_ascii_case("trace") {
        Some(LevelFilter::Trace)
    } else {
        None
    }
}

struct LogLine {
    bytes: [u8; 192],
    len: usize,
}

impl LogLine {
    const fn new() -> Self {
        Self {
            bytes: [0; 192],
            len: 0,
        }
    }

    fn finish(&mut self) {
        self.bytes[self.len] = b'\n';
        self.len += 1;
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl core::fmt::Write for LogLine {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let mut count = text.len().min(self.bytes.len() - 1 - self.len);
        while !text.is_char_boundary(count) {
            count -= 1;
        }
        self.bytes[self.len..self.len + count].copy_from_slice(&text.as_bytes()[..count]);
        self.len += count;
        if count == text.len() {
            Ok(())
        } else {
            Err(core::fmt::Error)
        }
    }
}

async fn authenticate(socket: &mut TcpSocket<'_>) -> bool {
    let mut line = [0_u8; 192];
    let mut len = 0;
    while len < line.len() {
        match socket.read(&mut line[len..len + 1]).await {
            Ok(1) if line[len] == b'\n' => {
                let Ok(text) = core::str::from_utf8(&line[..len]) else {
                    return false;
                };
                let mut fields = text.split_ascii_whitespace();
                return fields.next() == Some("FMDULOG1")
                    && fields.next() == Some(OTA_TOKEN)
                    && fields.next().is_none();
            }
            Ok(1) => len += 1,
            _ => return false,
        }
    }
    false
}

#[embassy_executor::task]
pub async fn server_task(stack: Stack<'static>) -> ! {
    if OTA_TOKEN.is_empty() || OTA_TOKEN == "change-me" || LOG_PORT == 0 {
        log::error!("WLAN logs disabled: set a non-default OTA_TOKEN and LOG_PORT");
        core::future::pending::<()>().await;
        unreachable!();
    }
    let mut rx = [0_u8; 256];
    let mut tx = [0_u8; 512];
    log::info!("WLAN log server listening on port {LOG_PORT}");
    loop {
        stack.wait_config_up().await;
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        if socket.accept(LOG_PORT).await.is_err() {
            Timer::after(Duration::from_millis(250)).await;
            continue;
        }
        socket.set_timeout(Some(Duration::from_secs(10)));
        if !matches!(
            with_timeout(Duration::from_secs(10), authenticate(&mut socket)).await,
            Ok(true)
        ) {
            let _ = with_timeout(
                Duration::from_secs(2),
                socket.write_all(b"ERR authentication\n"),
            )
            .await;
            socket.abort();
            continue;
        }
        if !matches!(
            with_timeout(
                Duration::from_secs(5),
                socket.write_all(b"OK FreeMDU live logs\n")
            )
            .await,
            Ok(Ok(()))
        ) {
            socket.abort();
            continue;
        }
        // A quiet logger should stay connected. Bound writes separately so
        // stalled clients never retain the only log socket indefinitely.
        socket.set_timeout(None);
        let mut peer = [0_u8; 1];
        loop {
            let line = match select(socket.read(&mut peer), LINES.receive()).await {
                Either::First(_) => break, // disconnect or unexpected client data
                Either::Second(line) => line,
            };
            let dropped = DROPPED.lock(|count| count.replace(0));
            if dropped != 0 {
                let mut notice = LogLine::new();
                let _ = write!(
                    &mut notice,
                    "WARN - {dropped} log records dropped (buffer full)"
                );
                notice.finish();
                if !matches!(
                    with_timeout(Duration::from_secs(5), socket.write_all(notice.as_bytes())).await,
                    Ok(Ok(()))
                ) {
                    break;
                }
            }
            if !matches!(
                with_timeout(Duration::from_secs(5), socket.write_all(line.as_bytes())).await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
        socket.abort();
    }
}
