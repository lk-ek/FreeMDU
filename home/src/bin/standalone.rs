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
    sync::atomic::{AtomicBool, Ordering},
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

const LOG_PORT: u16 = freemdu_home::num_from_env!("LOG_PORT", u16);
const LOG_AUTH_TIMEOUT: Duration = Duration::from_secs(10);

const DIAG_PORT: u16 = freemdu_home::num_from_env!("DIAG_PORT", u16);
const DIAG_AUTH_TIMEOUT: Duration = Duration::from_secs(10);

const BRIDGE_PORT: u16 = freemdu_home::num_from_env!("BRIDGE_PORT", u16);
const BRIDGE_AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const BRIDGE_CHUNK_SIZE: usize = 32;

/// MQTT topic used to report device availability
const STATUS_TOPIC: Topic<&str> = Topic::Device("status");

#[derive(Clone, Copy, Debug)]
enum DiagnosticCommand {
    QueryId,
    FindReadKey { start: u16, end: u16 },
    ReadMemory16 { key: u16, address: u32 },
    ReadEeprom16 { key: u16, address: u16 },
}

const DIAG_RESPONSE_CAPACITY: usize = 160;

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
    Data(BridgeChunk),
}

static BRIDGE_COMMANDS: Channel<CriticalSectionRawMutex, BridgeCommand, 4> = Channel::new();
static BRIDGE_EVENTS: Channel<CriticalSectionRawMutex, BridgeEvent, 8> = Channel::new();
static BRIDGE_ACTIVE: AtomicBool = AtomicBool::new(false);

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
) -> ! {
    let mut ticker = Ticker::every(DEVICE_PUBLISH_INTERVAL);
    let mut connected = false;
    let mut ir_debug_buf = [0_u8; 8];

    loop {
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
                    select::select(ticker.next(), port.debug_read_activity(&mut ir_debug_buf)),
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

                BRIDGE_ACTIVE.store(false, Ordering::Relaxed);
                ticker.reset();
                info!("Remote optical bridge released UART");
            }
            Either::First(_) => {
                // Stale bridge data/disconnect from a client that vanished
                // before ownership was established. Ignore it.
            }
            Either::Second(Either::First(command)) => {
                let response = execute_diagnostic_command(&mut port, command).await;
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
                }
            }
            Either::Second(Either::Second(Either::Second(Either::First(())))) if connected => {
                let state = match publish_device(&mut port, &hostname).await {
                    Ok(()) => AvailabilityState::Online,
                    Err(err) => {
                        error!("Failed to publish device: {err:#}");

                        AvailabilityState::Offline
                    }
                };

                if let Err(err) = STATUS_TOPIC.with_bytes(&state).publish().await {
                    error!("Failed to publish status: {err:?}");
                }
            }
            Either::Second(Either::Second(Either::Second(Either::Second(Ok(len))))) => {
                if len != 0 {
                    debug!("OPT AMBIENT RX {len}B {:x?}", &ir_debug_buf[..len]);
                }
            }
            Either::Second(Either::Second(Either::Second(Either::Second(Err(err))))) => {
                debug!("OPT AMBIENT UART activity/error: {err:?}");
            }
            _ => {}
        }

        led.set_level((!connected).into());
    }
}

async fn run_optical_bridge(port: &mut OpticalPort<'_>) {
    let mut rx_buf = [0_u8; BRIDGE_CHUNK_SIZE];

    loop {
        match select::select(BRIDGE_COMMANDS.receive(), port.read(&mut rx_buf)).await {
            Either::First(BridgeCommand::Data(chunk)) => {
                if let Err(err) = port.write(chunk.as_bytes()).await {
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
                debug!("Remote optical bridge UART activity/error: {err:?}");
            }
        }
    }
}

async fn execute_diagnostic_command(
    port: &mut OpticalPort<'_>,
    command: DiagnosticCommand,
) -> DiagnosticResponse {
    let mut response = DiagnosticResponse::new();

    match command {
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
        DiagnosticCommand::FindReadKey { start, end } => {
            info!("DIAG scanning read-access keys 0x{start:04x}..=0x{end:04x}");

            let mut intf = MieleInterface::new(&mut *port);
            match intf.query_software_id().with_timeout(DEVICE_TIMEOUT).await {
                Ok(Ok(id)) => info!("DIAG connected to software ID {id}"),
                Ok(Err(err)) => {
                    let _ = writeln!(&mut response, "ERR query_software_id {err:?}");
                    return response;
                }
                Err(err) => {
                    let _ = writeln!(&mut response, "ERR query_software_id timeout {err:?}");
                    return response;
                }
            }

            let mut found = None;

            for raw_key in u32::from(start)..=u32::from(end) {
                let key = raw_key as u16;

                if raw_key == u32::from(start) || raw_key % 0x0100 == 0 {
                    info!("DIAG read-key scan at 0x{key:04x}");
                }

                match intf
                    .unlock_read_access(key)
                    .with_timeout(DEVICE_TIMEOUT)
                    .await
                {
                    Ok(Ok(())) => {
                        found = Some(key);
                        break;
                    }
                    Ok(Err(_)) | Err(_) => {}
                }
            }

            if let Some(key) = found {
                info!("DIAG found read-access key 0x{key:04x}");
                let _ = writeln!(&mut response, "OK read_key=0x{key:04x}");
            } else {
                let _ = writeln!(
                    &mut response,
                    "NOT_FOUND start=0x{start:04x} end=0x{end:04x}"
                );
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
    }

    response
}

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
    // ID 410 (W307, ca. 2003) is not in the upstream device database yet.
    // Probe the software ID before using device::connect() and expose the
    // read-only fields that have already been verified against RAM snapshots.
    {
        let mut intf = MieleInterface::new(&mut *port);
        let software_id = intf
            .query_software_id()
            .with_timeout(DEVICE_TIMEOUT)
            .await
            .map_err(|err| anyhow::anyhow!("Failed to query software ID: {err:?}"))??;

        if software_id == 410 {
            return publish_w307_id410(&mut intf, hostname).await;
        }
    }

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

const W307_ID410_READ_KEY: u16 = 0x43ea;
const W307_ID410_DEVICE_NAME: &str = "Miele W307 (experimental ID 410)";

async fn publish_w307_id410(
    intf: &mut MieleInterface<&mut OpticalPort<'_>>,
    hostname: &str,
) -> Result<()> {
    intf.unlock_read_access(W307_ID410_READ_KEY)
        .with_timeout(DEVICE_TIMEOUT)
        .await
        .map_err(|err| anyhow::anyhow!("W307 read unlock timed out: {err:?}"))??;

    // Read a few compact regions rather than dozens of individual bytes.
    // The offsets below are either directly confirmed on ID 410 or are
    // conservative id360-derived fields that matched the idle/program-selected
    // snapshots. Everything remains strictly read-only.
    let op_a: [u8; 0x20] = intf
        .read_memory(0x009e)
        .with_timeout(DEVICE_TIMEOUT)
        .await
        .map_err(|err| anyhow::anyhow!("W307 operation block A timed out: {err:?}"))??;
    let op_b: [u8; 0x30] = intf
        .read_memory(0x00c8)
        .with_timeout(DEVICE_TIMEOUT)
        .await
        .map_err(|err| anyhow::anyhow!("W307 operation block B timed out: {err:?}"))??;
    let io_a: [u8; 0x10] = intf
        .read_memory(0x0078)
        .with_timeout(DEVICE_TIMEOUT)
        .await
        .map_err(|err| anyhow::anyhow!("W307 I/O block timed out: {err:?}"))??;
    let temp: [u8; 0x10] = intf
        .read_memory(0x0130)
        .with_timeout(DEVICE_TIMEOUT)
        .await
        .map_err(|err| anyhow::anyhow!("W307 temperature block timed out: {err:?}"))??;
    let motor: [u8; 0x10] = intf
        .read_memory(0x0280)
        .with_timeout(DEVICE_TIMEOUT)
        .await
        .map_err(|err| anyhow::anyhow!("W307 motor block timed out: {err:?}"))??;

    let display = decode_w307_display([op_a[0], op_a[1], op_a[2], op_a[3]]);
    let phase_raw = op_a[0x00a2 - 0x009e];
    let selected_program_raw = op_a[0x00b5 - 0x009e];

    let operating_state_raw = op_b[0x00cd - 0x00c8];
    let program_type_raw = op_b[0x00de - 0x00c8];
    let program_temperature = op_b[0x00df - 0x00c8];

    let active_actuators = u16::from_le_bytes([io_a[0x007d - 0x0078], io_a[0x007e - 0x0078]]);
    let water_level = io_a[0x007f - 0x0078];
    let water_level_target = io_a[0x0080 - 0x0078];

    let target_temperature = temp[0x0135 - 0x0130];
    let current_temperature = temp[0x0136 - 0x0130];

    let motor_pwm_raw = motor[0];
    let motor_pwm_percent = u16::from(motor_pwm_raw) * 100 / 0xff;

    let operating_state = w307_operating_state(operating_state_raw);
    let selected_program = w307_program(selected_program_raw);
    let program_type = w307_program_type(program_type_raw);
    let program_phase = w307_program_phase(phase_raw);

    publish_w307_sensor(
        hostname,
        "cycle_status",
        "Cycle status",
        None,
        operating_state,
    )
    .await?;
    publish_w307_sensor(
        hostname,
        "program_running",
        "Program running",
        None,
        if operating_state_raw == 2 {
            "Yes"
        } else {
            "No"
        },
    )
    .await?;
    publish_w307_sensor(
        hostname,
        "program_finished",
        "Program finished",
        None,
        if operating_state_raw == 3 {
            "Yes"
        } else {
            "No"
        },
    )
    .await?;
    publish_w307_sensor(
        hostname,
        "selected_program",
        "Selected program",
        None,
        selected_program,
    )
    .await?;
    publish_w307_sensor(hostname, "program_type", "Program type", None, program_type).await?;
    publish_w307_sensor(
        hostname,
        "program_phase",
        "Program phase",
        None,
        program_phase,
    )
    .await?;
    publish_w307_sensor(
        hostname,
        "program_temperature",
        "Selected temperature",
        Some("°C"),
        program_temperature,
    )
    .await?;
    publish_w307_sensor(
        hostname,
        "temperature",
        "Drum temperature",
        Some("°C"),
        current_temperature,
    )
    .await?;
    publish_w307_sensor(
        hostname,
        "target_temperature",
        "Target temperature",
        Some("°C"),
        target_temperature,
    )
    .await?;
    publish_w307_sensor(
        hostname,
        "display_contents",
        "Display contents",
        None,
        if display.is_empty() {
            "--"
        } else {
            display.as_str()
        },
    )
    .await?;
    publish_w307_sensor(
        hostname,
        "water_level",
        "Water level",
        Some("mmH₂O"),
        water_level,
    )
    .await?;
    publish_w307_sensor(
        hostname,
        "water_level_target",
        "Target water level",
        Some("mmH₂O"),
        water_level_target,
    )
    .await?;
    publish_w307_sensor(
        hostname,
        "motor_pwm_duty_cycle",
        "Motor PWM duty cycle",
        Some("%"),
        motor_pwm_percent,
    )
    .await?;
    publish_w307_sensor(
        hostname,
        "active_actuators_raw",
        "Active actuators (raw)",
        None,
        format!("0x{active_actuators:04x}"),
    )
    .await?;

    info!(
        "Published W307/ID410: state={operating_state} program={selected_program} \
         type={program_type} temp={program_temperature}°C phase={program_phase} \
         display={display:?}"
    );

    Ok(())
}

async fn publish_w307_sensor(
    hostname: &str,
    id: &str,
    name: &str,
    unit: Option<&str>,
    value: impl core::fmt::Display,
) -> Result<()> {
    let unique_id = format!("{hostname}_w307_{id}");
    let topic = Topic::Device(format!("w307/{id}/value"));

    Entity {
        device: HaDevice {
            name: Some(W307_ID410_DEVICE_NAME),
            ..HaDevice::default()
        },
        origin: Origin::default(),
        object_id: &unique_id,
        unique_id: Some(&unique_id),
        name,
        availability: AvailabilityTopics::All([STATUS_TOPIC]),
        state_topic: Some(topic.as_ref()),
        command_topic: None,
        component: Sensor {
            device_class: None,
            state_class: None,
            unit_of_measurement: unit,
        },
    }
    .publish_discovery()
    .await
    .map_err(|err| anyhow::anyhow!("Failed to publish W307 HA discovery for {id}: {err:?}"))?;

    topic
        .with_display(value)
        .publish()
        .await
        .map_err(|err| anyhow::anyhow!("Failed to publish W307 value {id}: {err:?}"))
}

fn w307_operating_state(value: u8) -> &'static str {
    match value {
        0 => "Door open",
        1 => "Ready",
        2 => "Running",
        3 => "Finished",
        4 => "Service programming",
        5 => "Customer programming",
        6 => "Service",
        _ => "Unknown",
    }
}

fn w307_program(value: u8) -> &'static str {
    match value {
        0 => "Finish",
        1 => "Cottons 95 °C",
        2 => "Cottons 75 °C",
        3 => "Cottons 60 °C",
        4 => "Cottons 40 °C",
        5 => "Cottons 30 °C",
        6 => "Minimum iron 60 °C",
        7 => "Minimum iron 50 °C",
        8 => "Minimum iron 40 °C",
        9 => "Minimum iron 30 °C",
        10 => "Drain/Spin",
        11 => "Separate rinse",
        12 => "Starch",
        13 => "Mixed wash 40 °C",
        14 => "Quick wash 40 °C",
        15 => "Woolens cold",
        16 => "Woolens 30 °C",
        17 => "Woolens 40 °C",
        18 => "Silks 30 °C",
        19 => "Delicates cold",
        20 => "Delicates 30 °C",
        21 => "Delicates 40 °C",
        _ => "Unknown",
    }
}

fn w307_program_type(value: u8) -> &'static str {
    match value {
        0x00 => "None",
        0x01 => "Cottons",
        0x02 => "Minimum iron",
        0x03 => "Delicates",
        0x04 => "Woolens",
        0x05 => "Quick wash",
        0x06 => "Starch",
        0x07 => "Drain/Spin",
        0x09 => "Separate rinse",
        0x0a => "Mixed wash",
        0x0b => "Silks",
        _ => "Unknown",
    }
}

fn w307_program_phase(value: u8) -> &'static str {
    match value {
        0 => "Idle",
        1 => "Delayed start",
        2 => "Soak/Pre-wash 1",
        3 => "Soak/Pre-wash 2",
        4 => "Main wash",
        5 => "Rinse 1",
        6 => "Rinse 2",
        7 => "Rinse 3",
        8 => "Rinse 4",
        9 => "Rinse 5",
        10 => "Rinse hold",
        11 => "Drain",
        12 => "Final spin",
        13 => "Anti-crease/Finish",
        _ => "Unknown",
    }
}

fn decode_w307_display(data: [u8; 4]) -> String {
    let points = (data[2] & 0x70) >> 4;
    let codes = [
        (data[0] & 0x0f, (data[3] & 0x02) != 0),
        ((data[0] & 0xf0) >> 4, (data[3] & 0x04) != 0),
        (data[1] & 0x0f, (data[3] & 0x08) != 0),
    ];

    let mut result = String::new();

    for (index, (code, special)) in codes.into_iter().enumerate() {
        if let Some(ch) = decode_w307_display_digit(code, special) {
            result.push(ch);
        }

        let digit = index as u8 + 1;
        if points == digit || points == 0x07 {
            result.push('.');
        }
    }

    result
}

fn decode_w307_display_digit(code: u8, special: bool) -> Option<char> {
    match (code, special) {
        (0x00, false) => Some('0'),
        (0x01, false) => Some('1'),
        (0x02, false) => Some('2'),
        (0x03, false) => Some('3'),
        (0x04, false) => Some('4'),
        (0x05, false) => Some('5'),
        (0x06, false) => Some('6'),
        (0x07, false) => Some('7'),
        (0x08, false) => Some('8'),
        (0x09, false) => Some('9'),
        (0x0a, false) => Some('A'),
        (0x0b, false) => Some('b'),
        (0x0c, false) => Some('C'),
        (0x0d, false) => Some('d'),
        (0x0e, false) => Some('E'),
        (0x0f, false) => Some('F'),
        (0x01, true) => Some('c'),
        (0x02, true) => Some('H'),
        (0x03, true) => Some('h'),
        (0x04, true) => Some('J'),
        (0x05, true) => Some('L'),
        (0x06, true) => Some('n'),
        (0x07, true) => Some('o'),
        (0x08, true) => Some('P'),
        (0x09, true) => Some('r'),
        (0x0a, true) => Some('U'),
        (0x0b, true) => Some('u'),
        (0x0c, true) => Some('y'),
        (0x0d, true) => Some('-'),
        (0x0e, true) => Some('='),
        (0x0f, true) => Some('°'),
        _ => None,
    }
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
async fn ota_server_task(stack: Stack<'static>, flash: FlashStorage<'static>) -> ! {
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

        if BRIDGE_ACTIVE.load(Ordering::Relaxed) {
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

        BRIDGE_COMMANDS.send(BridgeCommand::Connect).await;

        match BRIDGE_EVENTS.receive().await {
            BridgeEvent::Connected => {}
            BridgeEvent::Data(_) => {
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
            }
        }

        let _ = BRIDGE_COMMANDS.try_send(BridgeCommand::Disconnect);
        socket.abort();
    }
}

async fn run_diag_command(command: DiagnosticCommand) -> DiagnosticResponse {
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

    if kind == "eeprom" && end > 0x1ffff {
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
                // EEPROM protocol addresses are 16-bit words on these older
                // controllers; the serial CLI uses byte offsets for dumps.
                address: (offset / 2) as u16,
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
                 diag dump-eeprom KEY BYTE_START BYTE_END"
            );
        }
        Some("id") if fields.next().is_none() => {
            serial_diag_print_response(&run_diag_command(DiagnosticCommand::QueryId).await);
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
            Some("id") if fields.next().is_none() => Some(DiagnosticCommand::QueryId),
            Some("find-read-key") => {
                let start = fields.next().and_then(parse_diag_u16);
                let end = fields.next().and_then(parse_diag_u16);

                if fields.next().is_none() {
                    start
                        .zip(end)
                        .filter(|(start, end)| start <= end)
                        .map(|(start, end)| DiagnosticCommand::FindReadKey { start, end })
                } else {
                    None
                }
            }
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

#[embassy_executor::task]
async fn log_server_task(stack: Stack<'static>) -> ! {
    if OTA_TOKEN == "change-me" || OTA_TOKEN.is_empty() {
        error!("Network log console disabled: set a non-default OTA_TOKEN");
        core::future::pending::<()>().await;
        unreachable!();
    }

    let mut rx_buffer = [0_u8; 512];
    let mut tx_buffer = [0_u8; 1024];

    loop {
        stack.wait_config_up().await;

        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(LOG_AUTH_TIMEOUT));

        if let Err(err) = socket.accept(LOG_PORT).await {
            warn!("Log console accept failed: {err:?}");
            continue;
        }

        let mut header = [0_u8; 192];
        let mut header_len = 0_usize;
        let mut malformed = false;

        while header_len < header.len() {
            match socket.read(&mut header[header_len..]).await {
                Ok(0) => {
                    malformed = true;
                    break;
                }
                Ok(len) => {
                    header_len += len;
                    if header[..header_len].contains(&b'\n') {
                        break;
                    }
                }
                Err(_) => {
                    malformed = true;
                    break;
                }
            }
        }

        let newline = header[..header_len].iter().position(|byte| *byte == b'\n');
        let authenticated = newline
            .and_then(|index| core::str::from_utf8(&header[..index]).ok())
            .is_some_and(|line| {
                let mut fields = line.split_ascii_whitespace();
                fields.next() == Some("FMDULOG1")
                    && fields.next() == Some(OTA_TOKEN)
                    && fields.next().is_none()
            });

        if malformed || !authenticated {
            let _ = tcp_write_all(&mut socket, b"ERR invalid request or token\n").await;
            socket.close();
            continue;
        }

        if tcp_write_all(&mut socket, b"OK FreeMDU live logs\n")
            .await
            .is_err()
        {
            socket.abort();
            continue;
        }

        // A live log stream may legitimately be quiet for minutes. Keep the
        // authenticated connection open indefinitely. The host client can
        // reconnect after Wi-Fi drops, OTA updates or reboots.
        socket.set_timeout(None);
        info!(
            "Network log client connected from {:?}",
            socket.remote_endpoint()
        );

        loop {
            let line = netlog::next_line().await;
            if tcp_write_all(&mut socket, line.as_bytes()).await.is_err() {
                socket.abort();
                break;
            }
        }
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
    let flash = FlashStorage::new(peripherals.FLASH);
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
    spawner.spawn(mqtt_message_task(mqtt_receiver, hostname, port, led).unwrap());
    spawner.spawn(accelerometer_task(accelerometer, accel_hostname).unwrap());
    spawner.spawn(serial_diag_task(usb_serial).unwrap());
    spawner.spawn(network_stack_task(net_runner).unwrap());
    spawner.spawn(ota_server_task(net_stack, flash).unwrap());
    spawner.spawn(log_server_task(net_stack).unwrap());
    spawner.spawn(diagnostic_server_task(net_stack).unwrap());
    spawner.spawn(bridge_server_task(net_stack).unwrap());
    spawner.spawn(wifi_connect_task(wifi_controller).unwrap());
}
