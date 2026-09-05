use core::fmt::{self, Write as _};

use embassy_futures::select::{self, Either};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use log::{Level, LevelFilter, Log, Metadata, Record};

const ESP_LOG: &str = env!("ESP_LOG");
const LINE_CAPACITY: usize = 256;
// Keep important records separate from verbose DEBUG/TRACE traffic. A transient
// Wi-Fi outage can otherwise let optical DEBUG logging evict the beginning of
// an ID410 snapshot before the host reconnects.
const IMPORTANT_BACKLOG_LINES: usize = 96;
const VERBOSE_BACKLOG_LINES: usize = 16;

static IMPORTANT_LOG_CHANNEL: Channel<CriticalSectionRawMutex, LogLine, IMPORTANT_BACKLOG_LINES> =
    Channel::new();
static VERBOSE_LOG_CHANNEL: Channel<CriticalSectionRawMutex, LogLine, VERBOSE_BACKLOG_LINES> =
    Channel::new();
static LOGGER: NetLogger = NetLogger;

#[derive(Clone, Copy)]
pub struct LogLine {
    bytes: [u8; LINE_CAPACITY],
    len: usize,
}

impl LogLine {
    const fn new() -> Self {
        Self {
            bytes: [0; LINE_CAPACITY],
            len: 0,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl fmt::Write for LogLine {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let remaining = self.bytes.len().saturating_sub(self.len);
        if remaining == 0 {
            return Ok(());
        }

        let bytes = value.as_bytes();
        let count = remaining.min(bytes.len());
        self.bytes[self.len..self.len + count].copy_from_slice(&bytes[..count]);
        self.len += count;

        Ok(())
    }
}

struct NetLogger;

impl Log for NetLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        enabled_for(metadata.level(), metadata.target())
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        // Preserve the serial monitor output.
        esp_println::println!("{} - {}", record.level(), record.args());

        // Mirror a compact plain-text copy to the network console. Logging
        // must never block the firmware. Keep INFO/WARN/ERROR in a dedicated
        // backlog so high-rate optical DEBUG traffic cannot evict snapshots
        // while Wi-Fi is temporarily unavailable.
        let mut line = LogLine::new();
        let _ = writeln!(&mut line, "{} - {}", record.level(), record.args());

        if record.level() <= Level::Info {
            enqueue(&IMPORTANT_LOG_CHANNEL, line);
        } else {
            enqueue(&VERBOSE_LOG_CHANNEL, line);
        }
    }

    fn flush(&self) {}
}

fn enqueue<const N: usize>(
    channel: &'static Channel<CriticalSectionRawMutex, LogLine, N>,
    line: LogLine,
) {
    if let Err(embassy_sync::channel::TrySendError::Full(line)) = channel.try_send(line) {
        let _ = channel.try_receive();
        let _ = channel.try_send(line);
    }
}

pub fn init() {
    unsafe {
        log::set_logger_racy(&LOGGER).expect("logger already initialized");

        // Filtering is performed in `enabled_for`. Leave the global ceiling
        // at Trace so target-specific ESP_LOG directives can select any level.
        log::set_max_level_racy(LevelFilter::Trace);
    }
}

pub async fn next_line() -> LogLine {
    // Drain important records first when both queues already contain data.
    if let Ok(line) = IMPORTANT_LOG_CHANNEL.try_receive() {
        return line;
    }

    match select::select(
        IMPORTANT_LOG_CHANNEL.receive(),
        VERBOSE_LOG_CHANNEL.receive(),
    )
    .await
    {
        Either::First(line) | Either::Second(line) => line,
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
