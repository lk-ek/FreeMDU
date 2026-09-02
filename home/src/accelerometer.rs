//! Passive LIS2DH/LIS2DH12 vibration logger.
//!
//! This module intentionally contains no motion detection or control logic. It
//! samples acceleration and reduces it to windowed statistics suitable for
//! long-term storage in Home Assistant or another time-series database.

use esp_hal::{
    Async,
    gpio::AnyPin,
    i2c::master::{Config, ConfigError, Error as I2cError, I2c, Instance},
    time::Rate,
};

const WHO_AM_I: u8 = 0x0f;
const WHO_AM_I_VALUE: u8 = 0x33;
const CTRL_REG1: u8 = 0x20;
const CTRL_REG4: u8 = 0x23;
const OUT_X_L: u8 = 0x28;
const AUTO_INCREMENT: u8 = 0x80;

const ADDRESSES: [u8; 2] = [0x18, 0x19];

#[derive(Debug)]
pub enum Error {
    I2c(I2cError),
    DeviceNotFound,
    UnsupportedSampleRate(u32),
}

impl From<I2cError> for Error {
    fn from(value: I2cError) -> Self {
        Self::I2c(value)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    pub x_mg: i32,
    pub y_mg: i32,
    pub z_mg: i32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Metrics {
    pub samples: u32,
    pub x_mean_mg: i32,
    pub y_mean_mg: i32,
    pub z_mean_mg: i32,
    pub x_stddev_mg: u32,
    pub y_stddev_mg: u32,
    pub z_stddev_mg: u32,
    /// RMS of the AC component over all three axes. The per-window mean is
    /// removed before calculating the value, so gravity and mounting angle do
    /// not dominate it.
    pub vibration_rms_mg: u32,
    /// Largest peak-to-peak span of any individual axis in the window.
    pub peak_to_peak_mg: u32,
    /// Largest instantaneous 3-axis acceleration magnitude in the window.
    /// This includes gravity, so a stationary sensor reads approximately 1 g.
    pub acceleration_peak_mg: u32,
    /// Largest absolute excursion of any axis from that axis' window mean.
    /// This is a gravity/orientation-independent peak vibration indicator.
    pub dynamic_peak_mg: u32,
}

pub struct WindowStats {
    samples: u32,
    sum: [i64; 3],
    sum_sq: [u64; 3],
    min: [i32; 3],
    max: [i32; 3],
    max_magnitude_sq: u64,
}

impl WindowStats {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            samples: 0,
            sum: [0; 3],
            sum_sq: [0; 3],
            min: [i32::MAX; 3],
            max: [i32::MIN; 3],
            max_magnitude_sq: 0,
        }
    }

    pub fn push(&mut self, sample: Sample) {
        let vals = [sample.x_mg, sample.y_mg, sample.z_mg];
        self.samples += 1;

        let mut magnitude_sq = 0_u64;
        for (idx, value) in vals.into_iter().enumerate() {
            self.sum[idx] += i64::from(value);
            let value64 = i64::from(value);
            let square = (value64 * value64) as u64;
            self.sum_sq[idx] += square;
            magnitude_sq = magnitude_sq.saturating_add(square);
            self.min[idx] = self.min[idx].min(value);
            self.max[idx] = self.max[idx].max(value);
        }
        self.max_magnitude_sq = self.max_magnitude_sq.max(magnitude_sq);
    }

    #[must_use]
    pub fn finish(&self) -> Option<Metrics> {
        if self.samples == 0 {
            return None;
        }

        let n_i64 = i64::from(self.samples);
        let n_u64 = u64::from(self.samples);
        let n_sq = n_u64 * n_u64;
        let mut mean = [0_i32; 3];
        let mut variance = [0_u64; 3];

        for idx in 0..3 {
            mean[idx] = (self.sum[idx] / n_i64) as i32;

            // Population variance without first rounding the mean:
            //   var = (n * sum(x^2) - sum(x)^2) / n^2
            let sum_abs = self.sum[idx].unsigned_abs();
            let numerator = n_u64
                .saturating_mul(self.sum_sq[idx])
                .saturating_sub(sum_abs.saturating_mul(sum_abs));
            variance[idx] = numerator / n_sq;
        }

        let x_stddev = isqrt(variance[0]);
        let y_stddev = isqrt(variance[1]);
        let z_stddev = isqrt(variance[2]);
        let vibration_rms = isqrt(variance[0] + variance[1] + variance[2]);

        let mut peak_to_peak = 0_u32;
        let mut dynamic_peak = 0_u32;
        for idx in 0..3 {
            let span = i64::from(self.max[idx]) - i64::from(self.min[idx]);
            peak_to_peak = peak_to_peak.max(span.unsigned_abs().min(u64::from(u32::MAX)) as u32);

            let low_excursion = (i64::from(mean[idx]) - i64::from(self.min[idx])).unsigned_abs();
            let high_excursion = (i64::from(self.max[idx]) - i64::from(mean[idx])).unsigned_abs();
            dynamic_peak = dynamic_peak.max(
                low_excursion
                    .max(high_excursion)
                    .min(u64::from(u32::MAX)) as u32,
            );
        }

        Some(Metrics {
            samples: self.samples,
            x_mean_mg: mean[0],
            y_mean_mg: mean[1],
            z_mean_mg: mean[2],
            x_stddev_mg: x_stddev,
            y_stddev_mg: y_stddev,
            z_stddev_mg: z_stddev,
            vibration_rms_mg: vibration_rms,
            peak_to_peak_mg: peak_to_peak,
            acceleration_peak_mg: isqrt(self.max_magnitude_sq),
            dynamic_peak_mg: dynamic_peak,
        })
    }
}

impl Default for WindowStats {
    fn default() -> Self {
        Self::new()
    }
}

fn isqrt(value: u64) -> u32 {
    if value == 0 {
        return 0;
    }

    let mut x = value;
    let mut next = (x + value / x) / 2;
    while next < x {
        x = next;
        next = (x + value / x) / 2;
    }

    x.min(u64::from(u32::MAX)) as u32
}

pub fn new_i2c<'d>(i2c: impl Instance + 'd) -> Result<I2c<'d, Async>, ConfigError> {
    const PIN_SDA: u8 = crate::num_from_env!("PIN_ACCEL_SDA", u8);
    const PIN_SCL: u8 = crate::num_from_env!("PIN_ACCEL_SCL", u8);

    let i2c = I2c::new(i2c, Config::default().with_frequency(Rate::from_khz(400)))?
        .with_sda(unsafe { AnyPin::steal(PIN_SDA) })
        .with_scl(unsafe { AnyPin::steal(PIN_SCL) })
        .into_async();

    Ok(i2c)
}

pub struct Lis2dh<'d> {
    i2c: I2c<'d, Async>,
    address: Option<u8>,
}

impl<'d> Lis2dh<'d> {
    #[must_use]
    pub const fn new(i2c: I2c<'d, Async>) -> Self {
        Self { i2c, address: None }
    }

    pub async fn init(&mut self, sample_hz: u32) -> Result<u8, Error> {
        let address = self.probe().await?;

        // ODR + X/Y/Z enable. We use normal/high-resolution mode, not LP mode.
        let odr = match sample_hz {
            1 => 0x10,
            10 => 0x20,
            25 => 0x30,
            50 => 0x40,
            100 => 0x50,
            200 => 0x60,
            400 => 0x70,
            other => return Err(Error::UnsupportedSampleRate(other)),
        };
        self.write_reg(address, CTRL_REG1, odr | 0x07).await?;

        // BDU=1, FS=01 (+/-4 g), HR=1. In high-resolution +/-4 g mode the
        // output sensitivity is 2 mg/LSB after shifting the 12-bit sample.
        self.write_reg(address, CTRL_REG4, 0x98).await?;
        self.address = Some(address);

        Ok(address)
    }

    async fn probe(&mut self) -> Result<u8, Error> {
        for address in ADDRESSES {
            match self.read_reg(address, WHO_AM_I).await {
                Ok(WHO_AM_I_VALUE) => return Ok(address),
                Ok(_) | Err(_) => {}
            }
        }

        Err(Error::DeviceNotFound)
    }

    pub async fn read_sample(&mut self) -> Result<Sample, Error> {
        let address = self.address.ok_or(Error::DeviceNotFound)?;
        let mut raw = [0_u8; 6];

        self.i2c
            .write_read_async(address, &[OUT_X_L | AUTO_INCREMENT], &mut raw)
            .await?;

        let x = i16::from_le_bytes([raw[0], raw[1]]) >> 4;
        let y = i16::from_le_bytes([raw[2], raw[3]]) >> 4;
        let z = i16::from_le_bytes([raw[4], raw[5]]) >> 4;

        Ok(Sample {
            x_mg: i32::from(x) * 2,
            y_mg: i32::from(y) * 2,
            z_mg: i32::from(z) * 2,
        })
    }

    async fn read_reg(&mut self, address: u8, reg: u8) -> Result<u8, I2cError> {
        let mut value = [0_u8; 1];
        self.i2c
            .write_read_async(address, &[reg], &mut value)
            .await?;
        Ok(value[0])
    }

    async fn write_reg(&mut self, address: u8, reg: u8, value: u8) -> Result<(), I2cError> {
        self.i2c.write_async(address, &[reg, value]).await
    }
}
