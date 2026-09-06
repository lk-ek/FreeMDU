#![no_std]

pub mod accelerometer;

use embedded_io_async::{ErrorType, Read, ReadExactError, Write};
use esp_hal::{
    Async,
    gpio::{AnyPin, Input, InputConfig, Level, Output, OutputConfig},
    uart::{Config, ConfigError, Instance, Parity, RxError, Uart},
};

#[macro_export]
macro_rules! num_from_env {
    ($name:literal, $type:ty) => {
        match <$type>::from_str_radix(env!($name), 10) {
            Ok(val) => val,
            Err(_) => panic!("failed to parse environment variable as number"),
        }
    };
}

#[derive(Debug)]
pub enum OpticalError {
    Receive(RxError),
    Transmit,
    EchoTimeout,
    EmptyRead,
    EchoMismatch,
    NoisyLine,
    LateInput,
}

impl core::fmt::Display for OpticalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "optical transport: {self:?}")
    }
}
impl core::error::Error for OpticalError {}
impl embedded_io_async::Error for OpticalError {
    fn kind(&self) -> embedded_io_async::ErrorKind {
        embedded_io_async::ErrorKind::Other
    }
}

#[derive(Clone, Copy, Default)]
pub struct OpticalProgress {
    pub tx: usize,
    pub rx: usize,
}

pub struct OpticalPort<'a>(Uart<'a, Async>, OpticalProgress);

impl OpticalPort<'_> {
    /// Perform one unfiltered UART read for idle optical activity debugging.
    ///
    /// Like protocol reads, this exposes errors immediately. A consumer can
    /// see framing/parity errors caused by arbitrary IR sources such as a
    /// household remote control, which does not speak Miele's 2400-8E1 UART.
    pub async fn debug_read_activity(&mut self, buf: &mut [u8]) -> Result<usize, RxError> {
        self.0.read_async(buf).await
    }

    #[must_use]
    pub fn progress(&self) -> OpticalProgress {
        self.1
    }

    /// Drain stale bytes until 30 ms of silence; bounded even with ambient IR.
    pub async fn drain_input(&mut self) -> Result<(), OpticalError> {
        use embassy_time::{Duration, Timer, with_timeout};
        with_timeout(Duration::from_millis(300), async {
            let mut buf = [0_u8; 32];
            let mut activity = false;
            loop {
                match with_timeout(Duration::from_millis(30), self.0.read_async(&mut buf)).await {
                    Err(_) => {
                        return if activity {
                            Err(OpticalError::LateInput)
                        } else {
                            Ok(())
                        };
                    }
                    Ok(Ok(_)) => activity = true,
                    Ok(Err(_)) => {
                        activity = true;
                        Timer::after(Duration::from_millis(1)).await;
                    }
                }
            }
        })
        .await
        .map_err(|_| OpticalError::NoisyLine)?
    }

    /// No TX for longer than the appliance's three-second session timeout.
    /// Drain delayed responses while waiting, then require a quiet input.
    pub async fn resynchronize(&mut self) -> Result<(), OpticalError> {
        use embassy_time::{Duration, Timer, with_timeout};
        let _ = with_timeout(Duration::from_millis(3200), async {
            let mut buf = [0_u8; 32];
            loop {
                let _ = self.0.read_async(&mut buf).await;
                Timer::after(Duration::from_millis(1)).await;
            }
        })
        .await;
        self.drain_input().await
    }

    async fn read_raw(&mut self, buf: &mut [u8]) -> Result<usize, OpticalError> {
        if buf.is_empty() {
            return Ok(0);
        }
        match self
            .0
            .read_async(buf)
            .await
            .map_err(OpticalError::Receive)?
        {
            0 => Err(OpticalError::EmptyRead),
            len => Ok(len),
        }
    }
}

impl ErrorType for OpticalPort<'_> {
    type Error = OpticalError;
}

impl Read for OpticalPort<'_> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let len = self.read_raw(buf).await?;
        self.1.rx = self.1.rx.wrapping_add(len);
        log::debug!("OPT RX {len}B {:x?}", &buf[..len]);
        Ok(len)
    }

    async fn read_exact(&mut self, mut buf: &mut [u8]) -> Result<(), ReadExactError<Self::Error>> {
        while !buf.is_empty() {
            let len = self.read(buf).await?;

            buf = &mut buf[len..];
        }

        Ok(())
    }
}

impl Write for OpticalPort<'_> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let len = self.0.write_async(buf).await.map_err(|err| {
            log::warn!("OPT UART write error: {err:?}");
            OpticalError::Transmit
        })?;
        log::debug!("OPT TX {len}B {:x?}", &buf[..len]);

        // The SFH7250 receiver sees our own transmitted light. FreeMDU has
        // always consumed one echoed byte per transmitted byte here. Keep
        // doing that, but trace the echo separately from actual appliance RX.
        //
        // Diagnostic interpretation:
        //   TX with no subsequent ECHO -> local TX/RX optical path is broken
        //                                (or the outer protocol timeout fired)
        //   ECHO OK, then no RX         -> adapter can see itself, but the
        //                                appliance did not answer
        //   ECHO OK followed by RX      -> physical link is bidirectional
        let mut echo_ok = true;
        for (index, expected) in buf[..len].iter().copied().enumerate() {
            let mut echo = [0_u8; 1];
            embassy_time::with_timeout(
                embassy_time::Duration::from_millis(100),
                self.read_raw(&mut echo),
            )
            .await
            .map_err(|_| OpticalError::EchoTimeout)??;

            if echo[0] != expected {
                echo_ok = false;
                log::warn!(
                    "OPT ECHO mismatch at {}/{}: expected {:02x}, got {:02x}",
                    index + 1,
                    len,
                    expected,
                    echo[0]
                );
            }
        }

        if echo_ok {
            log::debug!("OPT ECHO {len}B OK");
        } else {
            log::warn!("OPT ECHO {len}B completed with mismatch(es)");
            return Err(OpticalError::EchoMismatch);
        }

        self.1.tx = self.1.tx.wrapping_add(len);
        Ok(len)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.0
            .flush_async()
            .await
            .map_err(|_| OpticalError::Transmit)
    }
}

#[must_use]
pub fn new_status_led<'a>() -> Output<'a> {
    const PIN: u8 = num_from_env!("PIN_LED_STATUS", u8);
    let led = unsafe { AnyPin::steal(PIN) };

    Output::new(led, Level::High, OutputConfig::default())
}

pub fn new_optical_port<'a>(uart: impl Instance + 'a) -> Result<OpticalPort<'a>, ConfigError> {
    const PIN_RX: u8 = num_from_env!("PIN_OPTICAL_RX", u8);
    const PIN_TX: u8 = num_from_env!("PIN_OPTICAL_TX", u8);
    let rx = Input::new(unsafe { AnyPin::steal(PIN_RX) }, InputConfig::default());
    let tx = Output::new(
        unsafe { AnyPin::steal(PIN_TX) },
        Level::Low,
        OutputConfig::default(),
    );
    let cfg = Config::default()
        .with_baudrate(2400)
        .with_parity(Parity::Even);
    let uart = Uart::new(uart, cfg)?
        .with_rx(rx.peripheral_input().with_input_inverter(true))
        .with_tx(tx.into_peripheral_output().with_output_inverter(true))
        .into_async();

    Ok(OpticalPort(uart, OpticalProgress::default()))
}
