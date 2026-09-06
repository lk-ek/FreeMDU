#![no_std]
#![no_main]

extern crate alloc;

mod netlog;

use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    vec::Vec,
};
use anyhow::{Context, Result};
use core::{
    fmt::Write,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use freemdu::embedded_io_async::Read;
use freemdu::embedded_io_async::Write as AsyncWrite;

use embassy_executor::Spawner;
use embassy_futures::select::{self, Either};
use embassy_net::{DhcpConfig, Runner, Stack, StackResources, tcp::TcpSocket};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel, mutex::Mutex};
use embassy_time::{Duration, Ticker, Timer, WithTimeout};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::system::software_reset;
use esp_hal::{
    efuse, gpio::Output, interrupt::software::SoftwareInterruptControl, peripherals::WIFI,
    rng::Rng, timer::timg::TimerGroup, usb_serial_jtag::UsbSerialJtag,
};
use esp_hal_ota::Ota;
use esp_radio::wifi::{
    self, ControllerConfig, CountryInfo, Interface, OperatingClass, WifiController,
    sta::StationConfig,
};
use esp_storage::FlashStorage;
use freemdu::Interface as MieleInterface;
use freemdu::device::{self, Action, ActionKind, Date, Property, PropertyKind, Value};
use freemdu_home::{
    OpticalPort,
    accelerometer::{Lis2dh, Metrics, WindowStats},
    keyscan::{Journal, Phase as ScanPhase, State as ScanState},
    shared_flash::{FlashMutex, SharedFlash},
};
use log::{debug, error, info, warn};
use mcutie::{
    McutieBuilder, McutieReceiver, McutieTask, MqttMessage, PublishBytes, Publishable, Topic,
    homeassistant::{
        AvailabilityState, AvailabilityTopics, Device as HaDevice, Entity, Origin, button::Button,
        sensor::Sensor,
    },
};
use static_cell::StaticCell;

// Interval for publishing device properties and actions
const DEVICE_PUBLISH_INTERVAL: Duration =
    Duration::from_secs(freemdu_home::num_from_env!("DEVICE_PUBLISH_INTERVAL", u64));

// Timeout for device operations (e.g. connection)
const DEVICE_TIMEOUT: Duration = Duration::from_secs(1);

// Delay between Wi-Fi reconnection attempts
const WIFI_RETRY_DELAY: Duration = Duration::from_secs(5);

const ACCEL_SAMPLE_HZ: u32 = freemdu_home::num_from_env!("ACCEL_SAMPLE_HZ", u32);
const ACCEL_PUBLISH_INTERVAL: u32 = freemdu_home::num_from_env!("ACCEL_PUBLISH_INTERVAL", u32);
const ACCEL_RETRY_DELAY: Duration = Duration::from_secs(1);

const OTA_PORT: u16 = freemdu_home::num_from_env!("OTA_PORT", u16);
const OTA_TOKEN: &str = env!("OTA_TOKEN");
const OTA_MAX_IMAGE_SIZE: usize = 0x1f0000;
const OTA_SOCKET_TIMEOUT: Duration = Duration::from_secs(30);

const DIAG_PORT: u16 = freemdu_home::num_from_env!("DIAG_PORT", u16);
const DIAG_AUTH_TIMEOUT: Duration = Duration::from_secs(10);

const BRIDGE_PORT: u16 = freemdu_home::num_from_env!("BRIDGE_PORT", u16);
const BRIDGE_AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const BRIDGE_CHUNK_SIZE: usize = 32;

// Read-only ID410 RAM tracing. A full 1 KiB sweep uses conservative 16-byte reads,
// matching the diagnostic path that has proven reliable on the W307.
const ID410_TRACE_INTERVAL: Duration =
    Duration::from_secs(freemdu_home::num_from_env!("ID410_TRACE_INTERVAL", u64));
const ID410_TRACE_RAM_SIZE: usize = 0x400;
const ID410_TRACE_BLOCK_SIZE: usize = 16;
const ID410_TRACE_BLOCK_TIMEOUT: Duration = Duration::from_secs(2);
const ID410_READ_KEY: u16 = 0x43ea;

// Read-access keys used by the device implementations currently included in
// FreeMDU. The generic first-contact probe only tries these known keys and
// performs reads; it never brute-forces arbitrary keys or writes appliance
// memory.
use freemdu::known_read_keys::KNOWN_READ_KEYS;

/// MQTT topic used to report device availability
const STATUS_TOPIC: Topic<&str> = Topic::Device("status");

#[derive(Clone, Copy, Debug)]
enum DiagnosticCommand {
    QueryId,
    QueryMaxBaud,
    ScanStatus,
    ScanPause,
    ScanResume,
    ScanReset,
    PartitionInstall,
    ScanStart {
        start: u16,
        end: u16,
        timeout_ms: u16,
        maximum_ms: u16,
    },
    ReadMemory16 {
        key: u16,
        address: u32,
    },
    ReadMemory128 {
        key: u16,
        address: u32,
    },
    ReadEeprom1 {
        key: u16,
        address: u16,
    },
    ReadEeprom16 {
        key: u16,
        address: u16,
    },
    ReadEeprom128 {
        key: u16,
        address: u16,
    },
}

const DIAG_RESPONSE_CAPACITY: usize = 512;

#[derive(Clone, Copy)]
struct DiagnosticResponse {
    bytes: [u8; DIAG_RESPONSE_CAPACITY],
    len: usize,
}

impl DiagnosticResponse {
    const fn new() -> Self {
        Self {
            bytes: [0; DIAG_RESPONSE_CAPACITY],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl core::fmt::Write for DiagnosticResponse {
    fn write_str(&mut self, value: &str) -> core::fmt::Result {
        let remaining = self.bytes.len().saturating_sub(self.len);
        let bytes = value.as_bytes();
        let count = remaining.min(bytes.len());
        self.bytes[self.len..self.len + count].copy_from_slice(&bytes[..count]);
        self.len += count;
        Ok(())
    }
}

static DIAG_COMMANDS: Channel<CriticalSectionRawMutex, DiagnosticCommand, 1> = Channel::new();
static DIAG_RESPONSES: Channel<CriticalSectionRawMutex, DiagnosticResponse, 1> = Channel::new();
static DIAG_REQUEST_LOCK: Mutex<CriticalSectionRawMutex, ()> = Mutex::new(());

static SCAN_STATE: embassy_sync::blocking_mutex::Mutex<
    CriticalSectionRawMutex,
    core::cell::Cell<ScanState>,
> = embassy_sync::blocking_mutex::Mutex::new(core::cell::Cell::new(ScanState::empty()));
static OTA_ACTIVE: AtomicBool = AtomicBool::new(false);
static SCAN_CURRENT: AtomicU32 = AtomicU32::new(0x10000);

struct ScanJob {
    flash: SharedFlash,
    journal: Option<Journal<SharedFlash>>,
    state: ScanState,
}
impl ScanJob {
    fn open(flash: SharedFlash) -> Self {
        let (journal, state) = match Journal::open(flash) {
            Ok((journal, state)) => (Some(journal), state),
            Err(err) => {
                error!("Key-scan storage unavailable: {err:?}");
                (
                    None,
                    ScanState {
                        phase: ScanPhase::StorageError,
                        ..ScanState::empty()
                    },
                )
            }
        };
        SCAN_STATE.lock(|s| s.set(state));
        Self {
            flash,
            journal,
            state,
        }
    }
    fn save(&mut self) -> bool {
        let saved = self
            .journal
            .as_mut()
            .is_some_and(|journal| journal.save(self.state).is_ok());
        if !saved {
            self.state.phase = ScanPhase::StorageError;
        }
        SCAN_STATE.lock(|s| s.set(self.state));
        saved
    }
}

fn scan_status() -> DiagnosticResponse {
    let s = SCAN_STATE.lock(core::cell::Cell::get);
    let mut response = DiagnosticResponse::new();
    let _ = write!(
        &mut response,
        "OK scan_version=3 state={} software_id={} start=0x{:04x} end=0x{:04x} next=0x{:05x} timeout_ms={} minimum_ms={} maximum_ms={} tested={} errors={} increases={}",
        s.phase.name(),
        s.software_id,
        s.start,
        s.end,
        s.next,
        s.timeout_ms,
        s.minimum_ms,
        s.maximum_ms,
        s.tested,
        s.errors,
        s.increases
    );
    let current = SCAN_CURRENT.load(Ordering::Relaxed);
    if current <= 0xffff {
        let _ = write!(&mut response, " current=0x{current:04x}");
    }
    if let Some(key) = s.found {
        let _ = write!(&mut response, " read_key=0x{key:04x} confirmed=2");
    }
    let _ = writeln!(&mut response);
    response
}

#[derive(Clone, Copy)]
struct BridgeChunk {
    bytes: [u8; BRIDGE_CHUNK_SIZE],
    len: usize,
}

impl BridgeChunk {
    const fn new() -> Self {
        Self {
            bytes: [0; BRIDGE_CHUNK_SIZE],
            len: 0,
        }
    }

    fn from_slice(data: &[u8]) -> Option<Self> {
        if data.len() > BRIDGE_CHUNK_SIZE {
            return None;
        }

        let mut chunk = Self::new();
        chunk.bytes[..data.len()].copy_from_slice(data);
        chunk.len = data.len();
        Some(chunk)
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

#[derive(Clone, Copy)]
enum BridgeCommand {
    Connect,
    Data(BridgeChunk),
    Disconnect,
}

#[derive(Clone, Copy)]
enum BridgeEvent {
    Connected,
    Disconnected,
    Data(BridgeChunk),
}

static BRIDGE_COMMANDS: Channel<CriticalSectionRawMutex, BridgeCommand, 4> = Channel::new();
static BRIDGE_EVENTS: Channel<CriticalSectionRawMutex, BridgeEvent, 8> = Channel::new();
static BRIDGE_ACTIVE: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
struct Id410Trace {
    checked_device: bool,
    enabled_for_device: bool,
    initialized: bool,
    snapshot_seq: u32,
    last_state: u8,
    last_phase: u8,
    observed_sweeps: u8,
    change_counts: [u8; ID410_TRACE_RAM_SIZE],
    shadow: [u8; ID410_TRACE_RAM_SIZE],
}

impl Id410Trace {
    const fn new() -> Self {
        Self {
            checked_device: false,
            enabled_for_device: false,
            initialized: false,
            snapshot_seq: 0,
            last_state: 0,
            last_phase: 0,
            observed_sweeps: 0,
            change_counts: [0; ID410_TRACE_RAM_SIZE],
            shadow: [0; ID410_TRACE_RAM_SIZE],
        }
    }
}

fn id410_trace_is_volatile(change_count: u8, observed_sweeps: u8) -> bool {
    // Require a few observations before classifying anything as noise. Once
    // enough history exists, an address is considered volatile when it changed
    // in at least a quarter of the successful comparison sweeps.
    observed_sweeps >= 4
        && change_count >= 2
        && u16::from(change_count) * 4 >= u16::from(observed_sweeps)
}

static MQTT_CONNECTED: AtomicBool = AtomicBool::new(false);

esp_bootloader_esp_idf::esp_app_desc!();

#[embassy_executor::task]
async fn mqtt_stack_task(
    task: McutieTask<
        'static,
        &'static str,
        PublishBytes<'static, &'static str, AvailabilityState>,
        1,
    >,
) -> ! {
    // Move large MQTT task to heap
    Box::pin(task.run()).await;
}

#[embassy_executor::task]
async fn mqtt_message_task(
    receiver: McutieReceiver,
    hostname: String,
    mut port: OpticalPort<'static>,
    mut led: Output<'static>,
    flash: SharedFlash,
) -> ! {
    let mut ticker = Ticker::every(DEVICE_PUBLISH_INTERVAL);
    let mut trace_ticker = Ticker::every(ID410_TRACE_INTERVAL);
    let mut id410_trace = Id410Trace::new();
    let mut connected = false;
    let mut ir_debug_buf = [0_u8; 8];
    let mut scan = ScanJob::open(flash);
    if scan.state.phase == ScanPhase::Running && port.resynchronize().await.is_err() {
        scan.state.failure();
        scan.save();
    }

    loop {
        if scan.state.phase == ScanPhase::Running {
            // Run one candidate at a time; command/status transport never owns
            // the job. Closing TCP does not cancel or restart it.
            if let Ok(BridgeCommand::Connect) = BRIDGE_COMMANDS.try_receive() {
                BRIDGE_EVENTS.send(BridgeEvent::Disconnected).await;
            }
            if let Ok(command) = DIAG_COMMANDS.try_receive() {
                let response = execute_diagnostic_command(&mut port, command, &mut scan).await;
                DIAG_RESPONSES.send(response).await;
            } else {
                netlog::set_quiet_diagnostic_scan(true);
                autonomous_scan_step(&mut port, &mut scan).await;
                netlog::set_quiet_diagnostic_scan(false);
            }
            Timer::after(Duration::from_millis(1)).await;
            continue;
        }
        // The remote bridge takes exclusive ownership of the optical UART.
        // MQTT polling, diagnostics and ambient-IR reads are suspended while a
        // bridge client is attached, matching the behaviour of the dedicated
        // USB bridge firmware and avoiding concurrent protocol transactions.
        match select::select(
            BRIDGE_COMMANDS.receive(),
            select::select(
                DIAG_COMMANDS.receive(),
                select::select(
                    receiver.receive(),
                    select::select(
                        ticker.next(),
                        select::select(
                            trace_ticker.next(),
                            port.debug_read_activity(&mut ir_debug_buf),
                        ),
                    ),
                ),
            ),
        )
        .await
        {
            Either::First(BridgeCommand::Connect) => {
                BRIDGE_ACTIVE.store(true, Ordering::Relaxed);
                info!("Remote optical bridge acquired UART");
                BRIDGE_EVENTS.send(BridgeEvent::Connected).await;

                run_optical_bridge(&mut port).await;
                BRIDGE_EVENTS.send(BridgeEvent::Disconnected).await;
                let _ = port.resynchronize().await;

                BRIDGE_ACTIVE.store(false, Ordering::Relaxed);
                ticker.reset();
                trace_ticker.reset();
                info!("Remote optical bridge released UART");
            }
            Either::First(_) => {
                // Stale bridge data/disconnect from a client that vanished
                // before ownership was established. Ignore it.
            }
            Either::Second(Either::First(command)) => {
                let response = execute_diagnostic_command(&mut port, command, &mut scan).await;
                if response.as_bytes().starts_with(b"ERR ") {
                    let _ = port.resynchronize().await;
                }
                DIAG_RESPONSES.send(response).await;
            }
            Either::Second(Either::Second(Either::First(MqttMessage::Connected))) => {
                connected = true;
                MQTT_CONNECTED.store(true, Ordering::Relaxed);
                ticker.reset();
            }
            Either::Second(Either::Second(Either::First(MqttMessage::Disconnected))) => {
                connected = false;
                MQTT_CONNECTED.store(false, Ordering::Relaxed);
            }
            Either::Second(Either::Second(Either::First(MqttMessage::Publish(
                Topic::Device(topic),
                payload,
            )))) => {
                if let Ok(param) = str::from_utf8(&payload)
                    && let Some((id, "trigger")) = topic.split_once('/')
                    && let Err(err) = trigger_action(&mut port, id, param).await
                {
                    error!("Failed to trigger action: {err:#}");
                    let _ = port.resynchronize().await;
                }
            }
            Either::Second(Either::Second(Either::Second(Either::First(())))) if connected => {
                let state = match publish_device(&mut port, &hostname).await {
                    Ok(()) => AvailabilityState::Online,
                    Err(err) => {
                        error!("Failed to publish device: {err:#}");
                        let _ = port.resynchronize().await;

                        AvailabilityState::Offline
                    }
                };

                if let Err(err) = STATUS_TOPIC.with_bytes(&state).publish().await {
                    error!("Failed to publish status: {err:?}");
                }
            }
            Either::Second(Either::Second(Either::Second(Either::Second(Either::First(()))))) => {
                if let Err(err) = trace_id410_memory(&mut port, &mut id410_trace).await {
                    warn!("ID410 TRACE sweep failed: {err:#}");
                    let _ = port.resynchronize().await;
                }
            }
            Either::Second(Either::Second(Either::Second(Either::Second(Either::Second(Ok(
                len,
            )))))) => {
                if len != 0 {
                    debug!("OPT AMBIENT RX {len}B {:x?}", &ir_debug_buf[..len]);
                }
            }
            Either::Second(Either::Second(Either::Second(Either::Second(Either::Second(
                Err(err),
            ))))) => {
                debug!("OPT AMBIENT UART activity/error: {err:?}");
            }
            _ => {}
        }

        led.set_level((!connected).into());
    }
}

async fn trace_id410_memory(port: &mut OpticalPort<'_>, trace: &mut Id410Trace) -> Result<()> {
    let mut intf = MieleInterface::new(&mut *port);

    if !trace.checked_device {
        let software_id = intf
            .query_software_id()
            .with_timeout(DEVICE_TIMEOUT)
            .await
            .map_err(|err| anyhow::anyhow!("software-ID query timed out: {err:?}"))??;

        trace.checked_device = true;
        trace.enabled_for_device = software_id == 410;

        if !trace.enabled_for_device {
            info!("ID410 TRACE disabled: connected appliance has software ID {software_id}");
            return Ok(());
        }

        info!(
            "ID410 TRACE enabled: read-only 0x0000..0x03ff sweep every {} s",
            ID410_TRACE_INTERVAL.as_secs()
        );
    }

    if !trace.enabled_for_device {
        return Ok(());
    }

    // The W307 controller needs a short idle gap after the normal property poll.
    // A trace ticker can become ready while MQTT polling owns the UART and would
    // otherwise run immediately afterwards. Starting another command train with
    // no quiet time has been observed to make the controller stop replying.
    Timer::after(Duration::from_millis(250)).await;

    // A new Interface needs the same handshake that already works reliably for
    // normal ID410 polling: query the software ID, then unlock read access.
    // A bare memory read here consistently times out on the W307 even when the
    // immediately preceding property poll was able to read memory.
    let software_id = intf
        .query_software_id()
        .with_timeout(DEVICE_TIMEOUT)
        .await
        .map_err(|err| anyhow::anyhow!("trace software-ID query timed out: {err:?}"))??;

    if software_id != 410 {
        trace.enabled_for_device = false;
        return Err(anyhow::anyhow!(
            "trace handshake unexpectedly returned software ID {software_id}"
        ));
    }

    intf.unlock_read_access(ID410_READ_KEY)
        .with_timeout(DEVICE_TIMEOUT)
        .await
        .map_err(|err| anyhow::anyhow!("trace read unlock timed out: {err:?}"))??;

    let mut current = [0_u8; ID410_TRACE_RAM_SIZE];

    for offset in (0..ID410_TRACE_RAM_SIZE).step_by(ID410_TRACE_BLOCK_SIZE) {
        let block: [u8; ID410_TRACE_BLOCK_SIZE] = match intf
            .read_memory(offset as u32)
            .with_timeout(ID410_TRACE_BLOCK_TIMEOUT)
            .await
        {
            Ok(Ok(block)) => block,
            Ok(Err(err)) => {
                return Err(anyhow::anyhow!("RAM read 0x{offset:04x} failed: {err:?}"));
            }
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "RAM read 0x{offset:04x} timed out: {err:?}"
                ));
            }
        };
        current[offset..offset + ID410_TRACE_BLOCK_SIZE].copy_from_slice(&block);

        // Avoid hammering the old controller with an uninterrupted request train.
        Timer::after(Duration::from_millis(5)).await;
    }

    let state = current[0x00cd];
    let phase = current[0x00a2];

    if !trace.initialized {
        trace.shadow.copy_from_slice(&current);
        trace.last_state = state;
        trace.last_phase = phase;
        trace.initialized = true;
        info!("ID410 TRACE baseline captured: state=0x{state:02x} phase=0x{phase:02x}");
        return Ok(());
    }

    // Keep transition detection independent from the RAM shadow. A failed
    // sweep must not be able to consume a state/phase edge, even if shadow
    // handling is changed later. These values are advanced only after a full
    // 1 KiB sweep completed successfully.
    let old_state = trace.last_state;
    let old_phase = trace.last_phase;
    let state_or_phase_changed = state != old_state || phase != old_phase;
    let observed_before = trace.observed_sweeps;
    let mut changed = 0_usize;
    let mut stable_changed = 0_usize;
    let mut volatile_changed = 0_usize;

    for (offset, (&old, &new)) in trace.shadow.iter().zip(current.iter()).enumerate() {
        if old != new {
            changed += 1;
            let volatile = id410_trace_is_volatile(trace.change_counts[offset], observed_before);

            if volatile {
                volatile_changed += 1;
            } else {
                stable_changed += 1;
            }

            info!("ID410 TRACE 0x{offset:04x} {old:02x}->{new:02x}");

            // On state/phase transitions, emit a second compact stream that
            // suppresses addresses which already proved noisy during preceding
            // successful sweeps. This keeps transition analysis readable while
            // preserving the full raw diff above.
            if state_or_phase_changed && !volatile {
                info!(
                    "ID410 STABLE 0x{offset:04x} {old:02x}->{new:02x} history={}/{}",
                    trace.change_counts[offset], observed_before
                );
            }
        }
    }

    if changed != 0 {
        info!(
            "ID410 TRACE sweep: {changed} byte(s) changed ({stable_changed} stable, {volatile_changed} volatile), state=0x{state:02x}, phase=0x{phase:02x}"
        );
    }

    if state_or_phase_changed {
        trace.snapshot_seq = trace.snapshot_seq.wrapping_add(1);
        let seq = trace.snapshot_seq;
        info!(
            "ID410 STABLE transition seq={seq}: {stable_changed} stable change(s), {volatile_changed} volatile change(s) suppressed"
        );
        info!(
            "ID410 SNAPSHOT BEGIN seq={seq} state={old_state:02x}->{state:02x} phase={old_phase:02x}->{phase:02x}"
        );

        for offset in (0..ID410_TRACE_RAM_SIZE).step_by(ID410_TRACE_BLOCK_SIZE) {
            let data = &current[offset..offset + ID410_TRACE_BLOCK_SIZE];
            info!("ID410 SNAPSHOT seq={seq} {offset:04x} {data:02x?}");
        }

        info!("ID410 SNAPSHOT END seq={seq}");
    }

    // Maintain a compact per-address churn history. Keep the counters bounded
    // to one byte per RAM address; when the observation window fills, decay all
    // counts by half so long washes keep a useful recent-history ratio instead
    // of saturating.
    if trace.observed_sweeps == u8::MAX {
        for count in &mut trace.change_counts {
            *count /= 2;
        }
        trace.observed_sweeps /= 2;
    }

    let observed_after = trace.observed_sweeps.saturating_add(1);
    for (offset, (&old, &new)) in trace.shadow.iter().zip(current.iter()).enumerate() {
        if old == new {
            continue;
        }

        let was_volatile =
            id410_trace_is_volatile(trace.change_counts[offset], trace.observed_sweeps);
        trace.change_counts[offset] = trace.change_counts[offset].saturating_add(1);
        let now_volatile = id410_trace_is_volatile(trace.change_counts[offset], observed_after);

        if !was_volatile && now_volatile {
            debug!(
                "ID410 TRACE volatile 0x{offset:04x}: {}/{} sweeps changed",
                trace.change_counts[offset], observed_after
            );
        }
    }
    trace.observed_sweeps = observed_after;

    // Commit all trace state atomically at the end of a successful sweep.
    // Any timeout/error above returns before touching the committed state, so
    // the next successful sweep still observes the pending transition.
    trace.shadow.copy_from_slice(&current);
    trace.last_state = state;
    trace.last_phase = phase;
    Ok(())
}

async fn run_optical_bridge(port: &mut OpticalPort<'_>) {
    let mut rx_buf = [0_u8; BRIDGE_CHUNK_SIZE];

    loop {
        match select::select(BRIDGE_COMMANDS.receive(), port.read(&mut rx_buf)).await {
            Either::First(BridgeCommand::Data(chunk)) => {
                if let Err(err) = port.write_all(chunk.as_bytes()).await {
                    warn!("Remote optical bridge TX failed: {err:?}");
                    return;
                }
            }
            Either::First(BridgeCommand::Disconnect) => return,
            Either::First(BridgeCommand::Connect) => {
                // Only one bridge client can own UART1 at a time.
            }
            Either::Second(Ok(len)) => {
                if len == 0 {
                    continue;
                }

                let Some(chunk) = BridgeChunk::from_slice(&rx_buf[..len]) else {
                    warn!("Remote optical bridge RX chunk too large: {len}");
                    continue;
                };

                BRIDGE_EVENTS.send(BridgeEvent::Data(chunk)).await;
            }
            Either::Second(Err(err)) => {
                warn!("Remote optical bridge UART error: {err:?}");
                return;
            }
        }
    }
}

async fn execute_diagnostic_command(
    port: &mut OpticalPort<'_>,
    command: DiagnosticCommand,
    scan: &mut ScanJob,
) -> DiagnosticResponse {
    let mut response = DiagnosticResponse::new();
    if scan.state.phase == ScanPhase::Running
        && !matches!(
            command,
            DiagnosticCommand::ScanStatus
                | DiagnosticCommand::ScanPause
                | DiagnosticCommand::ScanResume
                | DiagnosticCommand::ScanReset
                | DiagnosticCommand::ScanStart { .. }
        )
    {
        return diagnostic_error("ERR scan_busy");
    }

    match command {
        DiagnosticCommand::ScanStatus => return scan_status(),
        DiagnosticCommand::ScanPause => {
            if scan.state.phase == ScanPhase::Running {
                scan.state.phase = ScanPhase::Paused;
                scan.save();
            }
            return scan_status();
        }
        DiagnosticCommand::ScanResume => {
            if scan.state.phase == ScanPhase::Paused {
                if port.resynchronize().await.is_err() {
                    return diagnostic_error("ERR noisy_line");
                }
                scan.state.phase = ScanPhase::Running;
                scan.save();
            }
            return scan_status();
        }
        DiagnosticCommand::ScanReset => {
            scan.state = ScanState::empty();
            scan.save();
            return scan_status();
        }
        DiagnosticCommand::PartitionInstall => {
            if scan.state.phase == ScanPhase::Running || OTA_ACTIVE.load(Ordering::Relaxed) {
                return diagnostic_error("ERR busy pause_scan_and_finish_ota_first");
            }
            match freemdu_home::keyscan::install_partition(scan.flash) {
                Ok(()) => {
                    *scan = ScanJob::open(scan.flash);
                    return diagnostic_error("OK partition=keyscan installed_and_verified");
                }
                Err(err) => {
                    error!("Partition migration failed: {err:?}");
                    return diagnostic_error("ERR partition_migration_failed see_usb_log");
                }
            }
        }
        DiagnosticCommand::ScanStart {
            start,
            end,
            timeout_ms,
            maximum_ms,
        } => {
            if scan.journal.is_none() {
                return diagnostic_error("ERR scan_storage_unavailable");
            }
            // Repeated start is idempotent, including after a lost TCP reply.
            if scan.state.phase != ScanPhase::Idle {
                let same = scan.state.start == start
                    && scan.state.end == end
                    && scan.state.minimum_ms == timeout_ms
                    && scan.state.maximum_ms == maximum_ms;
                if !same {
                    return diagnostic_error("ERR different_scan use_scan_reset_first");
                }
                if scan.state.phase == ScanPhase::Paused {
                    if port.resynchronize().await.is_err() {
                        return diagnostic_error("ERR noisy_line");
                    }
                    scan.state.phase = ScanPhase::Running;
                    scan.save();
                }
                return scan_status();
            }
            if port.resynchronize().await.is_err() {
                return diagnostic_error("ERR noisy_line");
            }
            let id = {
                let mut intf = MieleInterface::new(&mut *port);
                match intf.query_software_id().with_timeout(DEVICE_TIMEOUT).await {
                    Ok(Ok(id)) => id,
                    _ => return diagnostic_error("ERR query_software_id timeout"),
                }
            };
            let Some(state) = ScanState::start(id, start, end, timeout_ms, maximum_ms) else {
                return diagnostic_error("ERR invalid_scan_arguments");
            };
            scan.state = state;
            scan.save();
            return scan_status();
        }
        DiagnosticCommand::QueryId => {
            let mut intf = MieleInterface::new(&mut *port);
            match intf.query_software_id().with_timeout(DEVICE_TIMEOUT).await {
                Ok(Ok(id)) => {
                    let _ = writeln!(&mut response, "OK software_id={id} hex=0x{id:04x}");
                }
                Ok(Err(err)) => {
                    let _ = writeln!(&mut response, "ERR query_software_id {err:?}");
                }
                Err(err) => {
                    let _ = writeln!(&mut response, "ERR query_software_id timeout {err:?}");
                }
            }
        }
        DiagnosticCommand::QueryMaxBaud => {
            let mut intf = MieleInterface::new(&mut *port);
            match intf
                .query_max_baud_rate()
                .with_timeout(DEVICE_TIMEOUT)
                .await
            {
                Ok(Ok(rate)) => {
                    let _ = writeln!(&mut response, "OK max_baud={}", rate.as_baud());
                }
                Ok(Err(err)) => {
                    let _ = writeln!(&mut response, "ERR query_max_baud_rate {err:?}");
                }
                Err(err) => {
                    let _ = writeln!(&mut response, "ERR query_max_baud_rate timeout {err:?}");
                }
            }
        }
        DiagnosticCommand::ReadMemory16 { key, address } => {
            let mut intf = MieleInterface::new(&mut *port);

            if let Err(err) = prepare_read_access(&mut intf, key).await {
                let _ = writeln!(&mut response, "{err}");
                return response;
            }

            match intf.read_memory(address).with_timeout(DEVICE_TIMEOUT).await {
                Ok(Ok(data)) => {
                    let data: [u8; 0x10] = data;
                    let _ = write!(
                        &mut response,
                        "OK kind=memory address=0x{address:08x} data="
                    );
                    for byte in data {
                        let _ = write!(&mut response, "{byte:02x}");
                    }
                    let _ = writeln!(&mut response);
                }
                Ok(Err(err)) => {
                    let _ = writeln!(&mut response, "ERR read_memory {err:?}");
                }
                Err(err) => {
                    let _ = writeln!(&mut response, "ERR read_memory timeout {err:?}");
                }
            }
        }
        DiagnosticCommand::ReadMemory128 { key, address } => {
            let mut intf = MieleInterface::new(&mut *port);

            if let Err(err) = prepare_read_access(&mut intf, key).await {
                let _ = writeln!(&mut response, "{err}");
                return response;
            }

            let mut data = [0u8; 0x80];
            for block in 0..8 {
                if block == 4 {
                    if let Err(err) = prepare_read_access(&mut intf, key).await {
                        let _ = writeln!(&mut response, "{err}");
                        return response;
                    }
                }

                let block_address = address + block as u32 * 0x10;
                match intf
                    .read_memory(block_address)
                    .with_timeout(DEVICE_TIMEOUT)
                    .await
                {
                    Ok(Ok(block_data)) => {
                        let block_data: [u8; 0x10] = block_data;
                        data[block * 0x10..(block + 1) * 0x10].copy_from_slice(&block_data);
                    }
                    Ok(Err(err)) => {
                        let _ = writeln!(
                            &mut response,
                            "ERR read_memory address=0x{block_address:08x} {err:?}"
                        );
                        return response;
                    }
                    Err(err) => {
                        let _ = writeln!(
                            &mut response,
                            "ERR read_memory timeout address=0x{block_address:08x} {err:?}"
                        );
                        return response;
                    }
                }

                // Avoid hammering old controllers with back-to-back reads.
                Timer::after(Duration::from_millis(5)).await;
            }

            let _ = write!(
                &mut response,
                "OK kind=memory address=0x{address:08x} data="
            );
            for byte in data {
                let _ = write!(&mut response, "{byte:02x}");
            }
            let _ = writeln!(&mut response);
        }
        DiagnosticCommand::ReadEeprom1 { key, address } => {
            let mut intf = MieleInterface::new(&mut *port);

            if let Err(err) = prepare_read_access(&mut intf, key).await {
                let _ = writeln!(&mut response, "{err}");
                return response;
            }

            match intf.read_eeprom(address).with_timeout(DEVICE_TIMEOUT).await {
                Ok(Ok(data)) => {
                    let data: [u8; 1] = data;
                    let _ = write!(
                        &mut response,
                        "OK kind=eeprom address=0x{address:04x} data="
                    );
                    for byte in data {
                        let _ = write!(&mut response, "{byte:02x}");
                    }
                    let _ = writeln!(&mut response);
                }
                Ok(Err(err)) => {
                    let _ = writeln!(&mut response, "ERR read_eeprom {err:?}");
                }
                Err(err) => {
                    let _ = writeln!(&mut response, "ERR read_eeprom timeout {err:?}");
                }
            }
        }
        DiagnosticCommand::ReadEeprom16 { key, address } => {
            let mut intf = MieleInterface::new(&mut *port);

            if let Err(err) = prepare_read_access(&mut intf, key).await {
                let _ = writeln!(&mut response, "{err}");
                return response;
            }

            match intf.read_eeprom(address).with_timeout(DEVICE_TIMEOUT).await {
                Ok(Ok(data)) => {
                    let data: [u8; 0x10] = data;
                    let _ = write!(
                        &mut response,
                        "OK kind=eeprom address=0x{address:04x} data="
                    );
                    for byte in data {
                        let _ = write!(&mut response, "{byte:02x}");
                    }
                    let _ = writeln!(&mut response);
                }
                Ok(Err(err)) => {
                    let _ = writeln!(&mut response, "ERR read_eeprom {err:?}");
                }
                Err(err) => {
                    let _ = writeln!(&mut response, "ERR read_eeprom timeout {err:?}");
                }
            }
        }
        DiagnosticCommand::ReadEeprom128 { key, address } => {
            let mut intf = MieleInterface::new(&mut *port);

            let address_unit = match intf.query_software_id().with_timeout(DEVICE_TIMEOUT).await {
                Ok(Ok(498)) => 1,
                Ok(Ok(_)) => 2,
                _ => return diagnostic_error("ERR query_software_id failed"),
            };

            if let Err(err) = prepare_read_access(&mut intf, key).await {
                let _ = writeln!(&mut response, "{err}");
                return response;
            }

            let mut data = [0u8; 0x80];
            for block in 0..8 {
                if block == 4 {
                    if let Err(err) = prepare_read_access(&mut intf, key).await {
                        let _ = writeln!(&mut response, "{err}");
                        return response;
                    }
                }

                let block_address = address + block as u16 * (0x10 / address_unit);
                match intf
                    .read_eeprom(block_address)
                    .with_timeout(DEVICE_TIMEOUT)
                    .await
                {
                    Ok(Ok(block_data)) => {
                        let block_data: [u8; 0x10] = block_data;
                        data[block * 0x10..(block + 1) * 0x10].copy_from_slice(&block_data);
                    }
                    Ok(Err(err)) => {
                        let _ = writeln!(
                            &mut response,
                            "ERR read_eeprom address=0x{block_address:04x} {err:?}"
                        );
                        return response;
                    }
                    Err(err) => {
                        let _ = writeln!(
                            &mut response,
                            "ERR read_eeprom timeout address=0x{block_address:04x} {err:?}"
                        );
                        return response;
                    }
                }

                // Avoid hammering old controllers with back-to-back reads.
                Timer::after(Duration::from_millis(5)).await;
            }

            let _ = write!(
                &mut response,
                "OK kind=eeprom address=0x{address:04x} data="
            );
            for byte in data {
                let _ = write!(&mut response, "{byte:02x}");
            }
            let _ = writeln!(&mut response);
        }
    }

    response
}

fn diagnostic_error(message: &str) -> DiagnosticResponse {
    let mut response = DiagnosticResponse::new();
    let _ = writeln!(&mut response, "{message}");
    response
}

// One clean handshake and one read. False means no appliance bytes arrived
// AFTER the separately timed read request and its echo completed correctly.
async fn probe_read_key(
    port: &mut OpticalPort<'_>,
    expected_id: u16,
    key: u16,
    timeout_ms: u64,
) -> Result<bool> {
    {
        let mut intf = MieleInterface::new(&mut *port);
        let id = intf
            .query_software_id()
            .with_timeout(DEVICE_TIMEOUT)
            .await
            .map_err(|_| anyhow::anyhow!("diagnostic timeout"))??;
        if id != expected_id {
            return Err(SoftwareIdChanged.into());
        }
        intf.unlock_read_access(key)
            .with_timeout(DEVICE_TIMEOUT)
            .await
            .map_err(|_| anyhow::anyhow!("diagnostic timeout"))??;
    }
    {
        let mut intf = MieleInterface::new(&mut *port);
        intf.begin_read_probe(0)
            .with_timeout(DEVICE_TIMEOUT)
            .await
            .map_err(|_| anyhow::anyhow!("read request/echo timeout"))??;
    }
    let before = port.progress();
    let result = {
        let mut intf = MieleInterface::new(&mut *port);
        intf.finish_read_probe()
            .with_timeout(Duration::from_millis(timeout_ms))
            .await
    };
    match result {
        Ok(Ok(_)) => Ok(true),
        Ok(Err(err)) => Err(anyhow::anyhow!("read transport/protocol error: {err:?}")),
        Err(_) => {
            let after = port.progress();
            if after.tx == before.tx && after.rx == before.rx {
                Ok(false)
            } else {
                Err(anyhow::anyhow!("partial read/echo timeout"))
            }
        }
    }
}

async fn confirm_read_key(port: &mut OpticalPort<'_>, id: u16, key: u16) -> Result<()> {
    // Independently relock through inactivity before EACH confirmation.
    // Do not compare RAM values: they can legitimately change between reads.
    for _ in 0..2 {
        port.resynchronize().await?;
        if !probe_read_key(port, id, key, 1000).await? {
            return Err(anyhow::anyhow!("candidate did not confirm"));
        }
        let mut intf = MieleInterface::new(&mut *port);
        let _: [u8; 16] = intf
            .read_memory(0)
            .with_timeout(DEVICE_TIMEOUT)
            .await
            .map_err(|_| anyhow::anyhow!("diagnostic timeout"))??;
    }
    Ok(())
}

async fn autonomous_scan_step(port: &mut OpticalPort<'_>, job: &mut ScanJob) {
    let mut candidate = None;
    for (index, entry) in KNOWN_READ_KEYS.iter().enumerate() {
        if index < 32
            && job.state.known_mask & (1 << index) == 0
            && entry.key >= job.state.start
            && entry.key <= job.state.end
        {
            candidate = Some((entry.key, Some(index)));
            break;
        }
    }
    if candidate.is_none() {
        while job.state.next <= u32::from(job.state.end) {
            let key = job.state.next as u16;
            if KNOWN_READ_KEYS.iter().enumerate().any(|(i, entry)| {
                i < 32 && entry.key == key && job.state.known_mask & (1 << i) != 0
            }) {
                job.state.next += 1;
            } else {
                candidate = Some((key, None));
                break;
            }
        }
    }
    let Some((key, known_index)) = candidate else {
        job.state.phase = ScanPhase::Done;
        job.save();
        return;
    };
    SCAN_CURRENT.store(u32::from(key), Ordering::Relaxed);
    let outcome: Result<bool> = async {
        for _ in 0..2 {
            if probe_read_key(
                port,
                job.state.software_id,
                key,
                u64::from(job.state.timeout_ms),
            )
            .await?
            {
                confirm_read_key(port, job.state.software_id, key).await?;
                return Ok(true);
            }
            // Late bytes or noise are errors, not a negative observation.
            port.drain_input().await?;
        }
        Ok(false)
    }
    .await;
    match outcome {
        Ok(true) => {
            job.state.found = Some(key);
            job.state.phase = ScanPhase::Found;
        }
        Ok(false) => {
            if let Some(i) = known_index {
                job.state.known_mask |= 1 << i;
            } else {
                job.state.next = u32::from(key) + 1;
            }
            job.state.tested = job.state.tested.saturating_add(1);
        }
        Err(err) => {
            warn!("Autonomous key 0x{key:04x}: {err:?}");
            if err.downcast_ref::<SoftwareIdChanged>().is_some() {
                job.state.errors = job.state.errors.saturating_add(1);
                job.state.phase = ScanPhase::Paused;
            } else {
                job.state.failure();
            }
            let _ = port.resynchronize().await;
        }
    }
    // Commit each completed candidate, timeout increase, pause or hit. A lost
    // power supply only repeats an uncommitted candidate.
    job.save();
    SCAN_CURRENT.store(0x10000, Ordering::Relaxed);
}

#[derive(Debug)]
struct SoftwareIdChanged;
impl core::fmt::Display for SoftwareIdChanged {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "software ID changed")
    }
}
impl core::error::Error for SoftwareIdChanged {}

async fn prepare_read_access(
    intf: &mut MieleInterface<&mut OpticalPort<'_>>,
    key: u16,
) -> Result<(), &'static str> {
    match intf.query_software_id().with_timeout(DEVICE_TIMEOUT).await {
        Ok(Ok(id)) => debug!("DIAG connected to software ID {id}"),
        Ok(Err(err)) => {
            warn!("DIAG query_software_id failed: {err:?}");
            return Err("ERR query_software_id");
        }
        Err(err) => {
            warn!("DIAG query_software_id timeout: {err:?}");
            return Err("ERR query_software_id timeout");
        }
    }

    match intf
        .unlock_read_access(key)
        .with_timeout(DEVICE_TIMEOUT)
        .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => {
            warn!("DIAG unlock_read_access failed: {err:?}");
            Err("ERR unlock_read_access")
        }
        Err(err) => {
            warn!("DIAG unlock_read_access timeout: {err:?}");
            Err("ERR unlock_read_access timeout")
        }
    }
}

#[embassy_executor::task]
async fn accelerometer_task(mut sensor: Lis2dh<'static>, hostname: String) -> ! {
    let sample_hz = ACCEL_SAMPLE_HZ.max(1);
    let sample_period = Duration::from_micros(1_000_000_u64 / u64::from(sample_hz));
    let target_samples = sample_hz.saturating_mul(ACCEL_PUBLISH_INTERVAL.max(1));
    let mut discovery_published = false;

    loop {
        let address = match sensor.init(sample_hz).await {
            Ok(address) => address,
            Err(err) => {
                error!("Failed to initialize LIS2DH: {err:?}");
                embassy_time::Timer::after(ACCEL_RETRY_DELAY).await;
                continue;
            }
        };

        info!("LIS2DH connected at I2C address 0x{address:02x}, sampling at {sample_hz} Hz");

        let mut ticker = Ticker::every(sample_period);
        let mut stats = WindowStats::new();

        loop {
            ticker.next().await;

            let sample = match sensor.read_sample().await {
                Ok(sample) => sample,
                Err(err) => {
                    warn!("Failed to read LIS2DH, reinitializing: {err:?}");
                    discovery_published = false;
                    embassy_time::Timer::after(ACCEL_RETRY_DELAY).await;
                    break;
                }
            };
            stats.push(sample);

            let Some(metrics) = stats.finish().filter(|m| m.samples >= target_samples) else {
                continue;
            };

            // mcutie uses QoS 0 by default and publishing while disconnected can
            // appear successful, so only mark discovery as sent while the MQTT
            // receiver reports an active connection.
            if !MQTT_CONNECTED.load(Ordering::Relaxed) {
                discovery_published = false;
                stats = WindowStats::new();
                continue;
            }

            if !discovery_published {
                match publish_accelerometer_discovery(&hostname).await {
                    Ok(()) => {
                        discovery_published = true;
                        info!("Published LIS2DH Home Assistant discovery");
                    }
                    Err(err) => debug!("Failed to publish LIS2DH discovery: {err:#}"),
                }
            }

            if let Err(err) = publish_accelerometer_metrics(&metrics).await {
                debug!("Failed to publish LIS2DH metrics: {err:#}");
            }

            stats = WindowStats::new();
        }
    }
}

async fn publish_accelerometer_discovery(hostname: &str) -> Result<()> {
    publish_accelerometer_sensor(hostname, "acceleration_x_mean", "Acceleration X mean", "mg")
        .await?;
    publish_accelerometer_sensor(hostname, "acceleration_y_mean", "Acceleration Y mean", "mg")
        .await?;
    publish_accelerometer_sensor(hostname, "acceleration_z_mean", "Acceleration Z mean", "mg")
        .await?;
    publish_accelerometer_sensor(hostname, "vibration_x_stddev", "Vibration X stddev", "mg")
        .await?;
    publish_accelerometer_sensor(hostname, "vibration_y_stddev", "Vibration Y stddev", "mg")
        .await?;
    publish_accelerometer_sensor(hostname, "vibration_z_stddev", "Vibration Z stddev", "mg")
        .await?;
    publish_accelerometer_sensor(hostname, "vibration_rms", "Vibration RMS", "mg").await?;
    publish_accelerometer_sensor(
        hostname,
        "acceleration_peak_to_peak",
        "Acceleration peak-to-peak",
        "mg",
    )
    .await?;

    // Human-readable values in g. The original mg statistics remain available
    // for lossless long-term analysis.
    publish_accelerometer_sensor(hostname, "acceleration_peak_g", "Peak acceleration", "g").await?;
    publish_accelerometer_sensor(hostname, "dynamic_peak_g", "Dynamic peak", "g").await?;
    publish_accelerometer_sensor(hostname, "vibration_rms_g", "Vibration RMS", "g").await?;
    publish_accelerometer_sensor(hostname, "peak_to_peak_g", "Peak-to-peak", "g").await?;

    Ok(())
}

async fn publish_accelerometer_sensor(
    hostname: &str,
    id: &str,
    name: &str,
    unit: &str,
) -> Result<()> {
    let unique_id = format!("{hostname}_lis2dh_{id}");
    let state_topic = Topic::Device(format!("accelerometer/{id}/value"));

    Entity {
        device: HaDevice {
            name: Some("FreeMDU vibration monitor"),
            ..HaDevice::default()
        },
        origin: Origin::default(),
        object_id: &unique_id,
        unique_id: Some(&unique_id),
        name,
        // Keep accelerometer availability independent of the Miele optical
        // interface. The sensor is intentionally useful even when no Miele
        // connection can be established.
        availability: AvailabilityTopics::<0>::None,
        state_topic: Some(state_topic.as_ref()),
        command_topic: None,
        component: Sensor {
            device_class: None,
            state_class: None,
            unit_of_measurement: Some(unit),
        },
    }
    .publish_discovery()
    .await
    .map_err(|err| anyhow::anyhow!("Failed to publish HA accelerometer sensor: {err:?}"))
}

async fn publish_accelerometer_metrics(metrics: &Metrics) -> Result<()> {
    publish_accelerometer_value("acceleration_x_mean", metrics.x_mean_mg).await?;
    publish_accelerometer_value("acceleration_y_mean", metrics.y_mean_mg).await?;
    publish_accelerometer_value("acceleration_z_mean", metrics.z_mean_mg).await?;
    publish_accelerometer_value("vibration_x_stddev", metrics.x_stddev_mg).await?;
    publish_accelerometer_value("vibration_y_stddev", metrics.y_stddev_mg).await?;
    publish_accelerometer_value("vibration_z_stddev", metrics.z_stddev_mg).await?;
    publish_accelerometer_value("vibration_rms", metrics.vibration_rms_mg).await?;
    publish_accelerometer_value("acceleration_peak_to_peak", metrics.peak_to_peak_mg).await?;

    publish_accelerometer_mg_as_g("acceleration_peak_g", metrics.acceleration_peak_mg).await?;
    publish_accelerometer_mg_as_g("dynamic_peak_g", metrics.dynamic_peak_mg).await?;
    publish_accelerometer_mg_as_g("vibration_rms_g", metrics.vibration_rms_mg).await?;
    publish_accelerometer_mg_as_g("peak_to_peak_g", metrics.peak_to_peak_mg).await?;

    Ok(())
}

async fn publish_accelerometer_mg_as_g(id: &str, value_mg: u32) -> Result<()> {
    // Avoid floating-point math while still publishing a normal decimal value
    // Home Assistant can graph directly, e.g. 83 mg -> "0.083" g.
    let value = format!("{}.{:03}", value_mg / 1000, value_mg % 1000);
    publish_accelerometer_value(id, value).await
}

async fn publish_accelerometer_value(id: &str, value: impl core::fmt::Display) -> Result<()> {
    Topic::Device(format!("accelerometer/{id}/value"))
        .with_display(value)
        .publish()
        .await
        .map_err(|err| anyhow::anyhow!("Failed to publish LIS2DH value {id}: {err:?}"))
}

async fn publish_device(port: &mut OpticalPort<'_>, hostname: &str) -> Result<()> {
    let mut dev = connect_to_device(port).await?;
    let dev_kind = dev.kind().to_string();
    let props = dev
        .properties()
        .iter()
        .filter(|prop| prop.kind == PropertyKind::Operation);
    let actions = dev
        .actions()
        .iter()
        .filter(|action| action.kind == ActionKind::Operation);
    let mut vals = Vec::with_capacity(props.clone().count());

    // Query properties first, as publishing them immediately might lead to timeout
    for prop in props.clone() {
        let val = dev
            .query_property(prop)
            .with_timeout(DEVICE_TIMEOUT)
            .await
            .map_err(|err| anyhow::anyhow!("Failed to query property: {err:?}"))??;

        info!("Queried property {prop:?} with value {val:?}");
        vals.push(val);
    }

    for (prop, val) in props.zip(vals) {
        publish_property(prop, &dev_kind, hostname).await?;
        publish_property_value(prop, &val).await?;
        info!("Published property: {prop:?}");
    }

    for action in actions {
        // There's no suitable HA component for actions with parameters
        if action.params.is_none() {
            publish_action(action, &dev_kind, hostname).await?;
            info!("Published action: {action:?}");
        } else {
            info!("Skipped action due to parameters: {action:?}");
        }
    }

    Ok(())
}

async fn publish_property(prop: &Property, dev: &str, hostname: &str) -> Result<()> {
    let unique_id = format!("{}_{}", hostname, prop.id);

    Entity {
        device: HaDevice {
            name: Some(dev),
            ..HaDevice::default()
        },
        origin: Origin::default(),
        object_id: &unique_id,
        unique_id: Some(&unique_id),
        name: prop.name,
        availability: AvailabilityTopics::All([STATUS_TOPIC]),
        state_topic: Some(Topic::Device(format!("{}/value", prop.id)).as_ref()),
        command_topic: None,
        component: Sensor {
            device_class: None,
            state_class: None,
            unit_of_measurement: prop.unit,
        },
    }
    .publish_discovery()
    .await
    .map_err(|err| anyhow::anyhow!("Failed to publish HA sensor: {err:?}"))
}

async fn publish_property_value(prop: &Property, val: &Value) -> Result<()> {
    let topic = Topic::Device(format!("{}/value", prop.id));

    match *val {
        Value::Number(num) => topic.with_display(num).publish().await,
        Value::Bool(val) => {
            topic
                .with_display(if val { "Yes" } else { "No" })
                .publish()
                .await
        }
        Value::String(ref string) => topic.with_display(string).publish().await,
        Value::Duration(dur) => {
            let total_mins = dur.as_secs() / 60;
            let hours = total_mins / 60;
            let mins = total_mins % 60;

            topic
                .with_display(format!("{hours}h {mins}min"))
                .publish()
                .await
        }
        Value::Date(Date { year, month, day }) => {
            topic
                .with_display(format!("{year}-{month:02}-{day:02}"))
                .publish()
                .await
        }
        // Sensor values and faults should not be published
        Value::Sensor(_, _) | Value::Fault(_) => Ok(()),
    }
    .map_err(|err| anyhow::anyhow!("Failed to publish property value: {err:?}"))
}

async fn publish_action(action: &Action, dev: &str, hostname: &str) -> Result<()> {
    let unique_id = format!("{}_{}", hostname, action.id);

    Entity {
        device: HaDevice {
            name: Some(dev),
            ..HaDevice::default()
        },
        origin: Origin::default(),
        object_id: &unique_id,
        unique_id: Some(&unique_id),
        name: action.name,
        availability: AvailabilityTopics::All([STATUS_TOPIC]),
        state_topic: None,
        command_topic: Some(Topic::Device(format!("{}/trigger", action.id)).as_ref()),
        component: Button { device_class: None },
    }
    .publish_discovery()
    .await
    .map_err(|err| anyhow::anyhow!("Failed to publish HA button: {err:?}"))
}

async fn trigger_action(port: &mut OpticalPort<'_>, id: &str, param: &str) -> Result<()> {
    let mut dev = connect_to_device(port).await?;

    let Some(action) = dev.actions().iter().find(|action| action.id == id) else {
        return Err(anyhow::anyhow!("Failed to find action with id {id}"));
    };

    info!("Triggering action {action:?} with parameter {param}");

    let param = if action.params.is_some() {
        Some(param)
    } else {
        None
    };

    Ok(dev
        .trigger_action(action, param)
        .with_timeout(DEVICE_TIMEOUT)
        .await
        .map_err(|err| anyhow::anyhow!("Failed to trigger action: {err:?}"))??)
}

async fn connect_to_device<'a, 'b>(
    port: &'a mut OpticalPort<'b>,
) -> Result<Box<dyn device::Device<&'a mut OpticalPort<'b>> + 'a>> {
    let dev = device::connect(port)
        .with_timeout(DEVICE_TIMEOUT)
        .await
        .map_err(|err| anyhow::anyhow!("Failed to connect to device: {err:?}"))??;

    info!(
        "Connected to device with kind {} and software ID {}",
        dev.kind(),
        dev.software_id()
    );

    Ok(dev)
}

#[embassy_executor::task]
async fn ota_server_task(stack: Stack<'static>, flash: SharedFlash) -> ! {
    if OTA_TOKEN == "change-me" || OTA_TOKEN.is_empty() {
        error!("OTA disabled: set a non-default OTA_TOKEN in .cargo/config.toml");
        core::future::pending::<()>().await;
        unreachable!();
    }

    let mut ota = match Ota::new(flash) {
        Ok(ota) => ota,
        Err(err) => {
            error!("Failed to initialize OTA support: {err:?}");
            core::future::pending::<()>().await;
            unreachable!();
        }
    };

    let mut rx_buffer = [0_u8; 4096];
    let mut tx_buffer = [0_u8; 256];

    loop {
        stack.wait_config_up().await;
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(OTA_SOCKET_TIMEOUT));

        if let Err(err) = socket.accept(OTA_PORT).await {
            warn!("OTA accept failed: {err:?}");
            continue;
        }
        info!("OTA client connected from {:?}", socket.remote_endpoint());

        let mut header = [0_u8; 256];
        let mut header_len = 0_usize;
        while header_len < header.len() {
            match socket.read(&mut header[header_len..header_len + 1]).await {
                Ok(0) => break,
                Ok(1) => {
                    header_len += 1;
                    if header[header_len - 1] == b'\n' {
                        break;
                    }
                }
                Ok(_) => unreachable!(),
                Err(err) => {
                    warn!("OTA header read failed: {err:?}");
                    break;
                }
            }
        }

        if header_len == 0 || header[header_len - 1] != b'\n' {
            let _ = socket.write(b"ERR malformed header\n").await;
            socket.close();
            continue;
        }
        let Ok(line) = core::str::from_utf8(&header[..header_len - 1]) else {
            let _ = socket.write(b"ERR header encoding\n").await;
            socket.close();
            continue;
        };
        let mut fields = line.split_ascii_whitespace();
        let magic = fields.next();
        let image_size = fields.next().and_then(|v| v.parse::<usize>().ok());
        let target_crc = fields.next().and_then(|v| u32::from_str_radix(v, 16).ok());
        let token = fields.next();
        if magic != Some("FMDU1")
            || image_size.is_none()
            || target_crc.is_none()
            || token != Some(OTA_TOKEN)
            || fields.next().is_some()
        {
            warn!("Rejected invalid OTA request");
            let _ = socket.write(b"ERR invalid request or token\n").await;
            socket.close();
            continue;
        }
        let image_size = image_size.unwrap();
        let target_crc = target_crc.unwrap();
        if image_size == 0 || image_size > OTA_MAX_IMAGE_SIZE {
            let _ = socket.write(b"ERR invalid image size\n").await;
            socket.close();
            continue;
        }

        struct ActiveOta;
        impl Drop for ActiveOta {
            fn drop(&mut self) {
                OTA_ACTIVE.store(false, Ordering::Relaxed);
            }
        }
        OTA_ACTIVE.store(true, Ordering::Relaxed);
        let _active = ActiveOta;
        info!("Starting OTA update: {image_size} bytes, CRC32 {target_crc:08x}");

        if let Err(err) = ota.ota_begin(image_size as u32, target_crc) {
            error!("Failed to start OTA update: {err:?}");

            let _ = socket.write_all(b"ERR ota_begin\n").await;
            socket.close();
            continue;
        }

        if socket.write_all(b"READY\n").await.is_err() {
            socket.abort();
            continue;
        }

        let mut remaining = image_size;
        let mut chunk = [0_u8; 4096];
        let mut complete = false;
        let mut failed = false;
        let mut last_percent = 0_u32;
        while remaining > 0 {
            let wanted = remaining.min(chunk.len());
            let n = match socket.read(&mut chunk[..wanted]).await {
                Ok(0) => {
                    failed = true;
                    break;
                }
                Ok(n) => n,
                Err(err) => {
                    warn!("OTA payload read failed: {err:?}");
                    failed = true;
                    break;
                }
            };
            match ota.ota_write_chunk(&chunk[..n]) {
                Ok(done) => complete = done,
                Err(err) => {
                    error!("OTA flash write failed: {err:?}");
                    failed = true;
                    break;
                }
            }
            remaining -= n;
            let percent = ((image_size - remaining) as u64 * 100 / image_size as u64) as u32;
            if percent >= last_percent + 10 || percent == 100 {
                info!("OTA progress: {percent}%");
                last_percent = percent;
            }
        }

        if failed || remaining != 0 || !complete {
            let _ = socket.write(b"ERR upload incomplete\n").await;
            socket.close();
            continue;
        }

        match ota.ota_flush(true, false) {
            Ok(()) => {
                info!("OTA image verified; rebooting");
                let _ = socket.write(b"OK verified; rebooting\n").await;
                let _ = socket.flush().await;
                embassy_time::Timer::after(Duration::from_millis(500)).await;
                software_reset();
            }
            Err(err) => {
                error!("OTA verification failed: {err:?}");
                let _ = socket.write(b"ERR CRC/flash verification failed\n").await;
                socket.close();
            }
        }
    }
}

#[embassy_executor::task]
async fn bridge_server_task(stack: Stack<'static>) -> ! {
    if OTA_TOKEN == "change-me" || OTA_TOKEN.is_empty() {
        error!("Remote optical bridge disabled: set a non-default OTA_TOKEN");
        core::future::pending::<()>().await;
        unreachable!();
    }

    let mut rx_buffer = [0_u8; 512];
    let mut tx_buffer = [0_u8; 512];

    loop {
        stack.wait_config_up().await;

        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(BRIDGE_AUTH_TIMEOUT));

        if let Err(err) = socket.accept(BRIDGE_PORT).await {
            warn!("Remote optical bridge accept failed: {err:?}");
            continue;
        }

        if BRIDGE_ACTIVE.load(Ordering::Relaxed)
            || SCAN_STATE.lock(|s| s.get().phase == ScanPhase::Running)
        {
            let _ = tcp_write_all(&mut socket, b"ERR bridge busy\n").await;
            socket.close();
            continue;
        }

        let mut header = [0_u8; 192];
        let mut header_len = 0_usize;

        while header_len < header.len() {
            match socket.read(&mut header[header_len..header_len + 1]).await {
                Ok(0) => break,
                Ok(1) => {
                    header_len += 1;
                    if header[header_len - 1] == b'\n' {
                        break;
                    }
                }
                Ok(_) => unreachable!(),
                Err(_) => break,
            }
        }

        if header_len == 0 || header[header_len - 1] != b'\n' {
            let _ = tcp_write_all(&mut socket, b"ERR malformed request\n").await;
            socket.close();
            continue;
        }

        let Ok(line) = core::str::from_utf8(&header[..header_len - 1]) else {
            let _ = tcp_write_all(&mut socket, b"ERR request encoding\n").await;
            socket.close();
            continue;
        };

        let mut fields = line.split_ascii_whitespace();
        let authenticated = fields.next() == Some("FMDUBRIDGE1")
            && fields.next() == Some(OTA_TOKEN)
            && fields.next().is_none();

        if !authenticated {
            let _ = tcp_write_all(&mut socket, b"ERR invalid token\n").await;
            socket.close();
            continue;
        }

        if SCAN_STATE.lock(|s| s.get().phase == ScanPhase::Running) {
            let _ = tcp_write_all(&mut socket, b"ERR scan busy\n").await;
            socket.close();
            continue;
        }
        BRIDGE_COMMANDS.send(BridgeCommand::Connect).await;

        match BRIDGE_EVENTS.receive().await {
            BridgeEvent::Connected => {}
            BridgeEvent::Data(_) | BridgeEvent::Disconnected => {
                let _ = BRIDGE_COMMANDS.try_send(BridgeCommand::Disconnect);
                socket.abort();
                continue;
            }
        }

        if tcp_write_all(&mut socket, b"OK FreeMDU optical bridge\n")
            .await
            .is_err()
        {
            let _ = BRIDGE_COMMANDS.try_send(BridgeCommand::Disconnect);
            socket.abort();
            continue;
        }

        socket.set_timeout(None);
        info!(
            "Remote optical bridge client connected from {:?}",
            socket.remote_endpoint()
        );

        let mut host_buf = [0_u8; BRIDGE_CHUNK_SIZE];

        loop {
            match select::select(socket.read(&mut host_buf), BRIDGE_EVENTS.receive()).await {
                Either::First(Ok(0)) => break,
                Either::First(Ok(len)) => {
                    let Some(chunk) = BridgeChunk::from_slice(&host_buf[..len]) else {
                        warn!("Remote optical bridge host chunk too large: {len}");
                        break;
                    };
                    BRIDGE_COMMANDS.send(BridgeCommand::Data(chunk)).await;
                }
                Either::First(Err(err)) => {
                    debug!("Remote optical bridge socket read ended: {err:?}");
                    break;
                }
                Either::Second(BridgeEvent::Data(chunk)) => {
                    if tcp_write_all(&mut socket, chunk.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Either::Second(BridgeEvent::Connected) => {}
                Either::Second(BridgeEvent::Disconnected) => break,
            }
        }

        let _ = BRIDGE_COMMANDS.try_send(BridgeCommand::Disconnect);
        socket.abort();
    }
}

async fn run_diag_command(command: DiagnosticCommand) -> DiagnosticResponse {
    if matches!(command, DiagnosticCommand::ScanStatus) {
        return scan_status();
    }
    let _guard = DIAG_REQUEST_LOCK.lock().await;
    DIAG_COMMANDS.send(command).await;
    DIAG_RESPONSES.receive().await
}

fn serial_diag_print_response(response: &DiagnosticResponse) {
    match core::str::from_utf8(response.as_bytes()) {
        Ok(text) => esp_println::println!("SERDIAG {}", text.trim_end()),
        Err(_) => esp_println::println!("SERDIAG ERR invalid response encoding"),
    }
}

async fn serial_diag_dump(kind: &str, key: u16, start: u32, end: u32) {
    if start > end || start % 16 != 0 || (end + 1) % 16 != 0 {
        esp_println::println!("SERDIAG ERR dump range must be 16-byte aligned and inclusive");
        return;
    }

    let mut address_unit = 2;
    if kind == "eeprom" {
        let reply = run_diag_command(DiagnosticCommand::QueryId).await;
        let id = core::str::from_utf8(reply.as_bytes())
            .ok()
            .and_then(|text| {
                text.strip_prefix("OK software_id=")?
                    .split_whitespace()
                    .next()?
                    .parse::<u16>()
                    .ok()
            });
        match id {
            Some(498) => address_unit = 1,
            Some(_) => (),
            None => {
                esp_println::println!("SERDIAG ERR query_software_id failed");
                return;
            }
        }
    }
    if kind == "eeprom" && end > 0x10000 * address_unit - 1 {
        esp_println::println!("SERDIAG ERR EEPROM byte offset out of range");
        return;
    }

    esp_println::println!(
        "SERDUMP BEGIN {} key=0x{:04x} start=0x{:08x} end=0x{:08x}",
        kind,
        key,
        start,
        end
    );

    let mut offset = start;
    while offset <= end {
        let command = if kind == "memory" {
            DiagnosticCommand::ReadMemory16 {
                key,
                address: offset,
            }
        } else {
            DiagnosticCommand::ReadEeprom16 {
                key,
                // The serial CLI uses byte offsets; ID498 is byte-addressed.
                address: (offset / address_unit) as u16,
            }
        };

        let response = run_diag_command(command).await;
        let Ok(text) = core::str::from_utf8(response.as_bytes()) else {
            esp_println::println!(
                "SERDUMP ERROR {} 0x{:08x} invalid-response-encoding",
                kind,
                offset
            );
            return;
        };

        let Some(data) = text.trim_end().split(" data=").nth(1) else {
            esp_println::println!(
                "SERDUMP ERROR {} 0x{:08x} {}",
                kind,
                offset,
                text.trim_end()
            );
            return;
        };

        esp_println::println!("SERDUMP {} {:08x} {}", kind, offset, data);
        offset += 16;

        // Yield so logging/MQTT/network tasks stay responsive during a dump.
        Timer::after(Duration::from_millis(1)).await;
    }

    esp_println::println!("SERDUMP END {}", kind);
}

async fn serial_diag_probe_unknown() {
    esp_println::println!("SERPROBE BEGIN read-only");

    // Query repeatedly so a marginal optical alignment is immediately visible
    // instead of being mistaken for an appliance/protocol mismatch.
    for sample in 1..=3 {
        let response = run_diag_command(DiagnosticCommand::QueryId).await;
        match core::str::from_utf8(response.as_bytes()) {
            Ok(text) => esp_println::println!("SERPROBE ID sample={} {}", sample, text.trim_end()),
            Err(_) => {
                esp_println::println!("SERPROBE ERROR invalid ID response encoding");
                return;
            }
        }
        Timer::after(Duration::from_millis(100)).await;
    }

    let mut selected_key = None;
    for candidate in KNOWN_READ_KEYS {
        let key = candidate.key;
        let response = run_diag_command(DiagnosticCommand::ReadMemory16 { key, address: 0 }).await;

        if response.as_bytes().starts_with(b"OK ") {
            esp_println::println!("SERPROBE READ_KEY key=0x{:04x} result=ok", key);
            selected_key = Some(key);
            break;
        }

        match core::str::from_utf8(response.as_bytes()) {
            Ok(text) => esp_println::println!(
                "SERPROBE READ_KEY key=0x{:04x} result=failed response={}",
                key,
                text.trim_end()
            ),
            Err(_) => esp_println::println!(
                "SERPROBE READ_KEY key=0x{:04x} result=failed invalid-response-encoding",
                key
            ),
        }
        Timer::after(Duration::from_millis(100)).await;
    }

    let Some(key) = selected_key else {
        esp_println::println!(
            "SERPROBE END no-known-read-key; use diag find-read-key only after reviewing the result"
        );
        return;
    };

    esp_println::println!(
        "SERPROBE BASELINE key=0x{:04x} memory=0x0000..0x03ff eeprom-bytes=0x0000..0x03ff",
        key
    );
    serial_diag_dump("memory", key, 0x0000, 0x03ff).await;
    serial_diag_dump("eeprom", key, 0x0000, 0x03ff).await;
    esp_println::println!("SERPROBE END key=0x{:04x}", key);
}

async fn handle_serial_diag_line(line: &str) {
    let mut fields = line.split_ascii_whitespace();

    if fields.next() != Some("diag") {
        return;
    }

    match fields.next() {
        Some("help") | None => {
            esp_println::println!(
                "SERDIAG usage: diag id | diag mem16 KEY ADDR | \
                 diag eeprom16 KEY WORD_ADDR | \
                 diag dump-memory KEY START END | \
                 diag dump-eeprom KEY BYTE_START BYTE_END | \
                 diag find-read-key START END [TIMEOUT_MS] | diag max-baud | diag probe"
            );
        }
        Some(
            name @ ("find-read-key" | "scan-start" | "scan-status" | "scan-pause" | "scan-resume"
            | "scan-reset" | "partition-install"),
        ) => {
            if let Some(command) = parse_scan_command(name, &mut fields) {
                serial_diag_print_response(&run_diag_command(command).await);
            } else {
                esp_println::println!("SERDIAG ERR invalid scan command");
            }
        }
        Some("id") if fields.next().is_none() => {
            serial_diag_print_response(&run_diag_command(DiagnosticCommand::QueryId).await);
        }
        Some("max-baud") if fields.next().is_none() => {
            serial_diag_print_response(&run_diag_command(DiagnosticCommand::QueryMaxBaud).await);
        }
        Some("probe") if fields.next().is_none() => {
            serial_diag_probe_unknown().await;
        }
        Some("mem16") => {
            let key = fields.next().and_then(parse_diag_u16);
            let address = fields.next().and_then(parse_diag_u32);

            if let (Some(key), Some(address), None) = (key, address, fields.next()) {
                serial_diag_print_response(
                    &run_diag_command(DiagnosticCommand::ReadMemory16 { key, address }).await,
                );
            } else {
                esp_println::println!("SERDIAG ERR usage: diag mem16 KEY ADDR");
            }
        }
        Some("eeprom1") => {
            let key = fields.next().and_then(parse_diag_u16);
            let address = fields.next().and_then(parse_diag_u16);

            if let (Some(key), Some(address), None) = (key, address, fields.next()) {
                serial_diag_print_response(
                    &run_diag_command(DiagnosticCommand::ReadEeprom1 { key, address }).await,
                );
            } else {
                esp_println::println!("SERDIAG ERR usage: diag eeprom1 KEY ADDR");
            }
        }
        Some("eeprom16") => {
            let key = fields.next().and_then(parse_diag_u16);
            let address = fields.next().and_then(parse_diag_u16);

            if let (Some(key), Some(address), None) = (key, address, fields.next()) {
                serial_diag_print_response(
                    &run_diag_command(DiagnosticCommand::ReadEeprom16 { key, address }).await,
                );
            } else {
                esp_println::println!("SERDIAG ERR usage: diag eeprom16 KEY WORD_ADDR");
            }
        }
        Some("dump-memory") => {
            let key = fields.next().and_then(parse_diag_u16);
            let start = fields.next().and_then(parse_diag_u32);
            let end = fields.next().and_then(parse_diag_u32);

            if let (Some(key), Some(start), Some(end), None) = (key, start, end, fields.next()) {
                serial_diag_dump("memory", key, start, end).await;
            } else {
                esp_println::println!("SERDIAG ERR usage: diag dump-memory KEY START END");
            }
        }
        Some("dump-eeprom") => {
            let key = fields.next().and_then(parse_diag_u16);
            let start = fields.next().and_then(parse_diag_u32);
            let end = fields.next().and_then(parse_diag_u32);

            if let (Some(key), Some(start), Some(end), None) = (key, start, end, fields.next()) {
                serial_diag_dump("eeprom", key, start, end).await;
            } else {
                esp_println::println!(
                    "SERDIAG ERR usage: diag dump-eeprom KEY BYTE_START BYTE_END"
                );
            }
        }
        Some(_) => {
            esp_println::println!("SERDIAG ERR unknown command; type: diag help");
        }
    }
}

#[embassy_executor::task]
async fn serial_diag_task(mut usb_serial: UsbSerialJtag<'static, esp_hal::Blocking>) -> ! {
    let mut line = [0_u8; 160];
    let mut len = 0_usize;

    info!("USB serial diagnostic console ready; type 'diag help'");

    loop {
        let mut received = false;

        while let Ok(byte) = usb_serial.read_byte() {
            received = true;

            match byte {
                b'\r' => {}
                b'\n' => {
                    if len != 0 {
                        if let Ok(command) = core::str::from_utf8(&line[..len]) {
                            handle_serial_diag_line(command).await;
                        } else {
                            esp_println::println!("SERDIAG ERR command is not UTF-8");
                        }
                        len = 0;
                    }
                }
                0x08 | 0x7f => {
                    len = len.saturating_sub(1);
                }
                byte if len < line.len() => {
                    line[len] = byte;
                    len += 1;
                }
                _ => {
                    len = 0;
                    esp_println::println!("SERDIAG ERR command line too long");
                }
            }
        }

        // read_byte() is non-blocking. Yield when the host has no data so the
        // USB console never stalls the Embassy executor.
        if !received {
            Timer::after(Duration::from_millis(10)).await;
        }
    }
}

#[embassy_executor::task]
async fn diagnostic_server_task(stack: Stack<'static>) -> ! {
    if OTA_TOKEN == "change-me" || OTA_TOKEN.is_empty() {
        error!("Diagnostic console disabled: set a non-default OTA_TOKEN");
        core::future::pending::<()>().await;
        unreachable!();
    }

    let mut rx_buffer = [0_u8; 384];
    let mut tx_buffer = [0_u8; 384];

    loop {
        stack.wait_config_up().await;

        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(DIAG_AUTH_TIMEOUT));

        if let Err(err) = socket.accept(DIAG_PORT).await {
            warn!("Diagnostic console accept failed: {err:?}");
            continue;
        }

        let mut line_buf = [0_u8; 192];
        let mut line_len = 0_usize;

        while line_len < line_buf.len() {
            match socket.read(&mut line_buf[line_len..line_len + 1]).await {
                Ok(0) => break,
                Ok(1) => {
                    line_len += 1;
                    if line_buf[line_len - 1] == b'\n' {
                        break;
                    }
                }
                Ok(_) => unreachable!(),
                Err(_) => break,
            }
        }

        if line_len == 0 || line_buf[line_len - 1] != b'\n' {
            let _ = tcp_write_all(&mut socket, b"ERR malformed request\n").await;
            socket.close();
            continue;
        }

        let Ok(line) = core::str::from_utf8(&line_buf[..line_len - 1]) else {
            let _ = tcp_write_all(&mut socket, b"ERR request encoding\n").await;
            socket.close();
            continue;
        };

        let mut fields = line.split_ascii_whitespace();

        if fields.next() != Some("FMDUDIAG1") || fields.next() != Some(OTA_TOKEN) {
            let _ = tcp_write_all(&mut socket, b"ERR invalid token\n").await;
            socket.close();
            continue;
        }

        let command = match fields.next() {
            Some(
                name @ ("find-read-key" | "scan-start" | "scan-status" | "scan-pause"
                | "scan-resume" | "scan-reset" | "partition-install"),
            ) => parse_scan_command(name, &mut fields),
            Some("id") if fields.next().is_none() => Some(DiagnosticCommand::QueryId),
            Some("max-baud") if fields.next().is_none() => Some(DiagnosticCommand::QueryMaxBaud),
            Some("mem16") => {
                let key = fields.next().and_then(parse_diag_u16);
                let address = fields.next().and_then(parse_diag_u32);

                if fields.next().is_none() {
                    key.zip(address)
                        .map(|(key, address)| DiagnosticCommand::ReadMemory16 { key, address })
                } else {
                    None
                }
            }
            Some("mem128") => {
                let key = fields.next().and_then(parse_diag_u16);
                let address = fields.next().and_then(parse_diag_u32);

                if fields.next().is_none() {
                    key.zip(address)
                        .map(|(key, address)| DiagnosticCommand::ReadMemory128 { key, address })
                } else {
                    None
                }
            }
            Some("eeprom1") => {
                let key = fields.next().and_then(parse_diag_u16);
                let address = fields.next().and_then(parse_diag_u16);

                if fields.next().is_none() {
                    key.zip(address)
                        .map(|(key, address)| DiagnosticCommand::ReadEeprom1 { key, address })
                } else {
                    None
                }
            }
            Some("eeprom16") => {
                let key = fields.next().and_then(parse_diag_u16);
                let address = fields.next().and_then(parse_diag_u16);

                if fields.next().is_none() && address.is_none_or(|address| address <= 0xfff0) {
                    key.zip(address)
                        .map(|(key, address)| DiagnosticCommand::ReadEeprom16 { key, address })
                } else {
                    None
                }
            }
            Some("eeprom128") => {
                let key = fields.next().and_then(parse_diag_u16);
                let address = fields.next().and_then(parse_diag_u16);

                if fields.next().is_none() && address.is_none_or(|address| address <= 0xff80) {
                    key.zip(address)
                        .map(|(key, address)| DiagnosticCommand::ReadEeprom128 { key, address })
                } else {
                    None
                }
            }
            _ => None,
        };

        let Some(command) = command else {
            let _ = tcp_write_all(
                &mut socket,
                b"ERR usage: id | find-read-key START END | mem16 KEY ADDR | eeprom16 KEY ADDR\n",
            )
            .await;
            socket.close();
            continue;
        };

        // The 10 s socket timeout is only for receiving/authenticating the
        // request. Long-running diagnostic commands (notably read-key scans)
        // must not be aborted merely because no TCP bytes are exchanged while
        // the optical transaction is in progress.
        socket.set_timeout(None);

        info!("DIAG request: {command:?}");
        let response = run_diag_command(command).await;

        match tcp_write_all(&mut socket, response.as_bytes()).await {
            Ok(()) => {
                if let Err(err) = socket.flush().await {
                    warn!("DIAG response flush failed: {err:?}");
                    socket.abort();
                    continue;
                }

                debug!("DIAG response sent: {} bytes", response.as_bytes().len());
                socket.close();
            }
            Err(err) => {
                warn!("DIAG response write failed: {err:?}");
                socket.abort();
            }
        }
    }
}

fn parse_scan_command(
    name: &str,
    fields: &mut core::str::SplitAsciiWhitespace<'_>,
) -> Option<DiagnosticCommand> {
    let command = match name {
        "scan-status" => DiagnosticCommand::ScanStatus,
        "scan-pause" => DiagnosticCommand::ScanPause,
        "scan-resume" => DiagnosticCommand::ScanResume,
        "scan-reset" => DiagnosticCommand::ScanReset,
        "partition-install" => DiagnosticCommand::PartitionInstall,
        "scan-start" | "find-read-key" => {
            let start = fields.next().and_then(parse_diag_u16)?;
            let end = fields.next().and_then(parse_diag_u16)?;
            let timeout_ms = fields.next().map_or(Some(100), parse_diag_u16)?;
            let maximum_ms = fields.next().map_or(Some(500), parse_diag_u16)?;
            ScanState::start(0, start, end, timeout_ms, maximum_ms)?;
            DiagnosticCommand::ScanStart {
                start,
                end,
                timeout_ms,
                maximum_ms,
            }
        }
        _ => return None,
    };
    fields.next().is_none().then_some(command)
}

fn parse_diag_u16(value: &str) -> Option<u16> {
    if let Some(value) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u16::from_str_radix(value, 16).ok()
    } else {
        value.parse().ok()
    }
}

fn parse_diag_u32(value: &str) -> Option<u32> {
    if let Some(value) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u32::from_str_radix(value, 16).ok()
    } else {
        value.parse().ok()
    }
}

async fn tcp_write_all(
    socket: &mut TcpSocket<'_>,
    mut data: &[u8],
) -> Result<(), embassy_net::tcp::Error> {
    while !data.is_empty() {
        let written = socket.write(data).await?;
        if written == 0 {
            return Err(embassy_net::tcp::Error::ConnectionReset);
        }
        data = &data[written..];
    }

    Ok(())
}

#[embassy_executor::task]
async fn network_stack_task(mut runner: Runner<'static, Interface<'static>>) -> ! {
    runner.run().await;
}

#[embassy_executor::task]
async fn wifi_connect_task(mut controller: WifiController<'static>) -> ! {
    loop {
        match controller.connect_async().await {
            Ok(info) => {
                info!("Wi-Fi connected: {info:?}");
                let info = controller.wait_for_disconnect_async().await;
                info!("Wi-Fi disconnected: {info:?}");
            }
            Err(err) => {
                error!("Failed to connect to Wi-Fi: {err:?}");
                embassy_time::Timer::after(WIFI_RETRY_DELAY).await;
            }
        }
    }
}

fn init_network(
    wifi: WIFI<'static>,
    hostname: &str,
) -> Result<(
    WifiController<'static>,
    Stack<'static>,
    Runner<'static, Interface<'static>>,
)> {
    static RESOURCES: StaticCell<StackResources<7>> = StaticCell::new();

    let (controller, intfs) = wifi::new(
        wifi,
        ControllerConfig::default()
            .with_initial_config(wifi::Config::Station(
                StationConfig::default()
                    .with_ssid(env!("WIFI_SSID"))
                    .with_password(env!("WIFI_PASSWORD").into()),
            ))
            .with_country_info(
                CountryInfo::from(*b"01").with_operating_class(OperatingClass::Indoors),
            ),
    )
    .context("Failed to configure and start Wi-Fi controller")?;

    let rng = Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    let mut cfg = DhcpConfig::default();

    cfg.hostname = Some(hostname.try_into().context("Failed to set DHCP hostname")?);

    let (stack, runner) = embassy_net::new(
        intfs.station,
        embassy_net::Config::dhcpv4(cfg),
        RESOURCES.init(StackResources::new()),
        seed,
    );

    Ok((controller, stack, runner))
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    netlog::init();

    let peripherals = esp_hal::init(esp_hal::Config::default());

    esp_alloc::heap_allocator!(size: 128 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);

    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    let mut hostname = String::with_capacity(32);

    write!(&mut hostname, "freemdu_home_").unwrap();

    for byte in efuse::base_mac_address().as_bytes() {
        write!(&mut hostname, "{byte:02x}").unwrap();
    }

    let led = freemdu_home::new_status_led();
    let port = freemdu_home::new_optical_port(peripherals.UART1).unwrap();
    let accel_i2c = freemdu_home::accelerometer::new_i2c(peripherals.I2C0).unwrap();
    let accelerometer = Lis2dh::new(accel_i2c);
    static FLASH: StaticCell<FlashMutex> = StaticCell::new();
    let flash = SharedFlash(FLASH.init(FlashMutex::new(core::cell::RefCell::new(
        FlashStorage::new(peripherals.FLASH),
    ))));
    let usb_serial = UsbSerialJtag::new(peripherals.USB_DEVICE);
    let accel_hostname = hostname.clone();
    let (wifi_controller, net_stack, net_runner) =
        init_network(peripherals.WIFI, &hostname).unwrap();
    let (mqtt_receiver, mqtt_task) =
        McutieBuilder::new(net_stack, "freemdu_home", env!("MQTT_HOSTNAME"))
            .with_authentication(env!("MQTT_USERNAME"), env!("MQTT_PASSWORD"))
            .with_subscriptions([Topic::Device("+/trigger")])
            .with_last_will(STATUS_TOPIC.with_bytes(AvailabilityState::Offline))
            .build();

    spawner.spawn(mqtt_stack_task(mqtt_task).unwrap());
    spawner.spawn(mqtt_message_task(mqtt_receiver, hostname, port, led, flash).unwrap());
    spawner.spawn(accelerometer_task(accelerometer, accel_hostname).unwrap());
    spawner.spawn(serial_diag_task(usb_serial).unwrap());
    spawner.spawn(network_stack_task(net_runner).unwrap());
    spawner.spawn(ota_server_task(net_stack, flash).unwrap());
    spawner.spawn(diagnostic_server_task(net_stack).unwrap());
    spawner.spawn(bridge_server_task(net_stack).unwrap());
    spawner.spawn(wifi_connect_task(wifi_controller).unwrap());
}
