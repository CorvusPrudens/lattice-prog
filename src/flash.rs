use super::sleep;
use anyhow::{Context, Ok, Result};
use rppal::gpio::{Gpio, InputPin, OutputPin};

#[allow(dead_code)]
pub struct FlashProgrammer {
    fpga_reset: OutputPin,
    fpga_cs: OutputPin,
    flash_cs: OutputPin,
    flash_sdi: OutputPin,
    flash_sdo: InputPin,
    flash_sck: OutputPin,
}

impl FlashProgrammer {
    const PROGRAM: u8 = 0x02;
    const READ: u8 = 0x03;
    #[allow(dead_code)]
    const WRITE_DISABLE: u8 = 0x04;
    const READ_STATUS_1: u8 = 0x05;
    const WRITE_ENABLE: u8 = 0x06;
    const BLOCK_ERASE: u8 = 0xD8;
    const WAKE: u8 = 0xAB;

    pub fn new() -> Result<Self> {
        let gpio = Gpio::new().with_context(|| "Failed to acquire GPIO")?;
        let mut fpga_reset = gpio
            .get(6)
            .with_context(|| "Failed to acquire FPGA reset pin")?
            .into_output_high();
        let mut fpga_cs = gpio
            .get(13)
            .with_context(|| "Failed to acquire FPGA CS pin")?
            .into_output_high();
        let flash_cs = gpio
            .get(5)
            .with_context(|| "Failed to acquire flash CS pin")?
            .into_output_high();
        let flash_sdi = gpio
            .get(10)
            .with_context(|| "Failed to acquire flash SDI")?
            .into_output_high();
        let flash_sck = gpio
            .get(10)
            .with_context(|| "Failed to acquire flash SCK")?
            .into_output_low();
        let flash_sdo = gpio
            .get(9)
            .with_context(|| "Failed to acquire flash SDO")?
            .into_input();

        // Here we allow the FPGA to reset and fail configuration, releasing the SPI bus
        sleep(1);
        // Set CRESET_B low for at least 200 ns, ensuring the FPGA's CS is low when reset is
        // released
        fpga_reset.set_low();
        fpga_cs.set_low();
        sleep(1);
        // Wait for at least 1200 us as the FPGA clears configuration memory
        fpga_reset.set_high();
        sleep(500);

        let mut programmer = Self {
            fpga_reset,
            fpga_cs,
            flash_cs,
            flash_sck,
            flash_sdi,
            flash_sdo,
        };

        programmer.write(Self::WAKE);

        Ok(programmer)
    }

    pub fn flash_data(&mut self, data: &[u8], address: usize) -> Result<()> {
        let mut address_offset = 0;

        let bar = indicatif::ProgressBar::new(data.len() as u64);

        for block in data.chunks(65536) {
            self.await_ready();
            self.erase_block(address + address_offset);

            for page in block.chunks(256) {
                self.await_ready();
                self.write_page(page, address + address_offset)?;
                address_offset += page.len();
                bar.inc(page.len() as u64);
            }
        }

        Ok(())
    }

    pub fn verify_data(&mut self, data: &[u8], address: usize) -> Result<()> {
        let mut address_offset = 0;

        let bar = indicatif::ProgressBar::new(data.len() as u64);
        self.await_ready();

        for input in data.chunks(256) {
            let read = self.read_page(address + address_offset);

            for (i, (input, read)) in input.iter().zip(read.iter()).enumerate() {
                if input != read {
                    anyhow::bail!(
                        "Verification error at page {}, index {}: expected {input} but got {read}",
                        address_offset / 256,
                        i + address_offset
                    );
                }
            }

            address_offset += input.len();
            bar.inc(input.len() as u64);
        }

        Ok(())
    }

    fn read(&mut self) -> u8 {
        let mut value = 0;
        for i in 0..8 {
            self.flash_sck.set_high();
            let level: u8 = matches!(self.flash_sdo.read(), rppal::gpio::Level::High) as u8;
            value |= level;
            if i < 7 {
                value <<= 1;
            }
            self.flash_sck.set_low();
        }
        value
    }

    fn status(&mut self) -> u8 {
        self.flash_cs.set_low();
        self.write(Self::READ_STATUS_1);
        let output = self.read();
        self.flash_cs.set_high();
        output
    }

    fn write_enable(&mut self) {
        self.flash_cs.set_low();
        self.write(Self::WRITE_ENABLE);
        self.flash_cs.set_high();
    }

    fn write(&mut self, byte: u8) {
        for i in (0..8).rev() {
            let level = (byte & (1 << i)) > 0;
            self.flash_sdi.write(level.into());

            self.flash_sck.set_low();
            self.flash_sck.set_high();
        }
    }

    fn write_address(&mut self, address: usize) {
        self.write((address >> 16) as u8);
        self.write((address >> 8) as u8);
        self.write(address as u8);
    }

    fn write_page(&mut self, data: &[u8], address: usize) -> anyhow::Result<()> {
        if data.len() > 256 {
            anyhow::bail!("Page data must not exceed 256 bytes");
        }

        self.write_enable();

        self.flash_cs.set_low();
        self.write(Self::PROGRAM);

        self.write_address(address);

        for byte in data {
            self.write(*byte);
        }
        self.flash_cs.set_high();

        Ok(())
    }

    fn read_page(&mut self, address: usize) -> [u8; 256] {
        let mut data = [0; 256];

        self.flash_cs.set_low();
        self.write(Self::READ);
        self.write_address(address);

        for byte in data.iter_mut() {
            *byte = self.read();
        }
        self.flash_cs.set_low();

        data
    }

    fn erase_block(&mut self, address: usize) {
        self.write_enable();

        self.flash_cs.set_low();
        self.write(Self::BLOCK_ERASE);
        self.write_address(address);
        self.flash_cs.set_high();
    }

    fn await_ready(&mut self) {
        while (self.status() & 1) > 0 {}
    }
}
