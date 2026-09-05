use core::sync::atomic::{AtomicBool, Ordering};
use log::{Level, LevelFilter, Log, Metadata, Record};

const ESP_LOG: &str = env!("ESP_LOG");
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

        // Keep diagnostics on USB Serial/JTAG only. Do not mirror log records
        // into RAM or expose a TCP log service; this keeps logging independent
        // of Wi-Fi quality and avoids a large static network-log backlog.
        esp_println::println!("{} - {}", record.level(), record.args());
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
