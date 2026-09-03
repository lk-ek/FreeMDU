use anyhow::{Context, Result};
use freemdu::Interface;
use freemdu::device::{self, Action, DeviceKind, Error, Property, PropertyKind, Value};

use crate::transport::Port;
use log::debug;
use tokio::{
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    task,
    time::{self, Duration},
};

// Timeout for device operations (e.g. connection)
const DEVICE_TIMEOUT: Duration = Duration::from_secs(1);

// Delay between device connection attempts
const DEVICE_CONNECT_INTERVAL: Duration = Duration::from_secs(4);

type Device<'a> = Box<dyn device::Device<&'a mut Port> + 'a>;

#[derive(Debug)]
pub enum Request {
    QueryProperties(PropertyKind),
    TriggerAction(&'static Action, Option<String>),
    RawQuerySoftwareId,
    RawUnlockRead { key: u16 },
    RawReadMemory16 { key: u16, address: u32 },
    RawReadEeprom16 { key: u16, address: u16 },
}

#[derive(Debug)]
pub enum Response {
    DeviceConnected {
        software_id: u16,
        kind: DeviceKind,
        actions: &'static [Action],
        tx: UnboundedSender<Request>,
    },
    UnknownDeviceConnected {
        software_id: u16,
        tx: UnboundedSender<Request>,
    },
    DeviceDisconnected,
    PropertiesQueried(PropertyKind, Vec<(&'static Property, Value)>),
    InvalidActionArgument(&'static Action),
    InvalidActionState(&'static Action),
    RawStatus(String),
    RawData {
        label: &'static str,
        data: Vec<u8>,
    },
}

pub struct Worker<'a> {
    dev: Device<'a>,
    tx: &'a UnboundedSender<Response>,
}

impl Worker<'_> {
    pub fn start(mut port: Port) -> UnboundedReceiver<Response> {
        let (tx, rx) = mpsc::unbounded_channel();
        task::spawn_local(async move {
            loop {
                // device::connect() may return a Device that borrows `port`.
                // Keep that result inside this scope so the borrow is fully
                // dropped before the unknown-device fallback borrows `port`.
                let use_raw_mode = {
                    match time::timeout(DEVICE_TIMEOUT, device::connect(&mut port)).await {
                        Ok(Ok(dev)) => {
                            let mut worker = Worker { dev, tx: &tx };
                            if let Err(err) = worker.run().await {
                                debug!("Error running device worker: {err:#}");
                            }
                            false
                        }
                        Ok(Err(err)) if err.to_string().contains("unknown software ID") => {
                            debug!("Unsupported device detected: {err:#}");
                            true
                        }
                        Ok(Err(err)) => {
                            debug!("Error connecting to device: {err:#}");
                            false
                        }
                        Err(_) => {
                            debug!("Device connection timed out");
                            false
                        }
                    }
                };

                if use_raw_mode && let Err(raw_err) = Self::run_unknown_device(&mut port, &tx).await
                {
                    debug!("Error running unknown-device worker: {raw_err:#}");
                }

                let _ = tx.send(Response::DeviceDisconnected);
                time::sleep(DEVICE_CONNECT_INTERVAL).await;
            }
        });
        rx
    }

    async fn run(&mut self) -> Result<()> {
        let (dev_tx, mut dev_rx) = mpsc::unbounded_channel();

        self.tx.send(Response::DeviceConnected {
            software_id: self.dev.software_id(),
            kind: self.dev.kind(),
            actions: self.dev.actions(),
            tx: dev_tx,
        })?;

        // Handle incoming commands from device channel
        while let Some(cmd) = dev_rx.recv().await {
            let res = match cmd {
                Request::QueryProperties(kind) => self
                    .query_properties(kind)
                    .await
                    .context("Failed to query properties"),
                Request::TriggerAction(action, param) => self
                    .trigger_action(action, param.as_deref())
                    .await
                    .context("Failed to trigger action"),
                Request::RawQuerySoftwareId
                | Request::RawUnlockRead { .. }
                | Request::RawReadMemory16 { .. }
                | Request::RawReadEeprom16 { .. } => Ok(()),
            };

            if res.is_err() {
                self.tx.send(Response::DeviceDisconnected)?;

                return res;
            }
        }

        Ok(())
    }

    async fn run_unknown_device(port: &mut Port, tx: &UnboundedSender<Response>) -> Result<()> {
        let mut intf = Interface::new(port);
        let software_id = time::timeout(DEVICE_TIMEOUT, intf.query_software_id())
            .await
            .context("Raw software-ID query timed out")??;
        let (req_tx, mut req_rx) = mpsc::unbounded_channel();
        tx.send(Response::UnknownDeviceConnected {
            software_id,
            tx: req_tx,
        })?;

        while let Some(request) = req_rx.recv().await {
            match request {
                Request::RawQuerySoftwareId => {
                    match time::timeout(DEVICE_TIMEOUT, intf.query_software_id()).await {
                        Ok(Ok(id)) => tx.send(Response::RawStatus(format!(
                            "Software ID: {id} / 0x{id:04x}"
                        )))?,
                        Ok(Err(err)) => tx.send(Response::RawStatus(format!(
                            "Software-ID query failed: {err}"
                        )))?,
                        Err(_) => {
                            tx.send(Response::RawStatus("Software-ID query timed out".into()))?
                        }
                    }
                }
                Request::RawUnlockRead { key } => {
                    let result = async {
                        intf.query_software_id().await?;
                        intf.unlock_read_access(key).await
                    };
                    match time::timeout(DEVICE_TIMEOUT, result).await {
                        Ok(Ok(())) => tx.send(Response::RawStatus(format!(
                            "Read access unlocked with key 0x{key:04x}"
                        )))?,
                        Ok(Err(err)) => {
                            tx.send(Response::RawStatus(format!("Read unlock failed: {err}")))?
                        }
                        Err(_) => tx.send(Response::RawStatus("Read unlock timed out".into()))?,
                    }
                }
                Request::RawReadMemory16 { key, address } => {
                    let result = async {
                        intf.query_software_id().await?;
                        intf.unlock_read_access(key).await?;
                        let data: [u8; 16] = intf.read_memory(address).await?;
                        Ok::<_, freemdu::Error<std::io::Error>>(data)
                    };
                    match time::timeout(DEVICE_TIMEOUT, result).await {
                        Ok(Ok(data)) => tx.send(Response::RawData {
                            label: "Memory @ 0x0000",
                            data: data.to_vec(),
                        })?,
                        Ok(Err(err)) => {
                            tx.send(Response::RawStatus(format!("Memory read failed: {err}")))?
                        }
                        Err(_) => tx.send(Response::RawStatus("Memory read timed out".into()))?,
                    }
                }
                Request::RawReadEeprom16 { key, address } => {
                    let result = async {
                        intf.query_software_id().await?;
                        intf.unlock_read_access(key).await?;
                        let data: [u8; 16] = intf.read_eeprom(address).await?;
                        Ok::<_, freemdu::Error<std::io::Error>>(data)
                    };
                    match time::timeout(DEVICE_TIMEOUT, result).await {
                        Ok(Ok(data)) => tx.send(Response::RawData {
                            label: "EEPROM @ word 0x0000",
                            data: data.to_vec(),
                        })?,
                        Ok(Err(err)) => {
                            tx.send(Response::RawStatus(format!("EEPROM read failed: {err}")))?
                        }
                        Err(_) => tx.send(Response::RawStatus("EEPROM read timed out".into()))?,
                    }
                }
                Request::QueryProperties(_) | Request::TriggerAction(_, _) => {}
            }
        }
        Ok(())
    }

    async fn query_properties(&mut self, kind: PropertyKind) -> Result<()> {
        let mut data = Vec::new();

        for prop in self
            .dev
            .properties()
            .iter()
            .filter(|prop| prop.kind == kind)
        {
            let val = time::timeout(DEVICE_TIMEOUT, self.dev.query_property(prop)).await??;

            data.push((prop, val));
        }

        self.tx.send(Response::PropertiesQueried(kind, data))?;

        Ok(())
    }

    async fn trigger_action(&mut self, action: &'static Action, param: Option<&str>) -> Result<()> {
        match time::timeout(DEVICE_TIMEOUT, self.dev.trigger_action(action, param)).await? {
            Err(Error::InvalidArgument) => self.tx.send(Response::InvalidActionArgument(action))?,
            Err(Error::InvalidState) => self.tx.send(Response::InvalidActionState(action))?,
            res => res?,
        }

        Ok(())
    }
}
