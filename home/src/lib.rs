#![no_std]

pub mod accelerometer;

use embedded_io_async::{ErrorType, Read, ReadExactError, Write};
use esp_hal::{
    Async,
    gpio::{AnyPin, Input, InputConfig, Level, Output, OutputConfig},
    uart::{Config, ConfigError, Instance, IoError, Parity, RxError, Uart},
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

pub struct OpticalPort<'a>(Uart<'a, Async>);

impl OpticalPort<'_> {
    /// Perform one unfiltered UART read for idle optical activity debugging.
    ///
    /// Unlike `read_raw`, this does not retry errors. A consumer can therefore
    /// see framing/parity errors caused by arbitrary IR sources such as a
    /// household remote control, which does not speak Miele's 2400-8E1 UART.
    pub async fn debug_read_activity(&mut self, buf: &mut [u8]) -> Result<usize, RxError> {
        self.0.read_async(buf).await
    }

    /// Read directly from the UART, retrying transient UART errors.
    ///
    /// This intentionally does not emit an RX trace. It is also used for the
    /// local optical echo consumed by `Write::write`, which must be
    /// distinguished from bytes received from the appliance.
    async fn read_raw(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        loop {
            match self.0.read_async(buf).await {
                Ok(0) => {
                    log::warn!("OPT UART returned an empty read; retrying");
                }
                Ok(len) => return Ok(len),
                Err(err) => {
                    log::warn!("OPT UART read error: {err:?}; retrying");
                }
            }
        }
    }
}

impl ErrorType for OpticalPort<'_> {
    type Error = IoError;
}

impl Read for OpticalPort<'_> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let len = self.read_raw(buf).await?;
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
        let len = self.0.write_async(buf).await?;
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
            self.read_raw(&mut echo).await?;

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
        }

        Ok(len)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(self.0.flush_async().await?)
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

    Ok(OpticalPort(uart))
}
