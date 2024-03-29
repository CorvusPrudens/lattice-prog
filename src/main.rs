//! A utility for programming lattice FPGAs in slave mode.
//! The documentation for configuration and programming can be found here:
//! https://www.latticesemi.com/view_document?document_id=46502
//!
//! This is intended for a Raspberry Pi, which is likely a 32-bit arm architecture (the
//! architecture may vary model-to-model).
//! To build, simply run `cross build --release --target armv7-unknown-linux-musleabihf`, or
//! whatever the correct target may be for the intended device.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use flash::FlashProgrammer;
use rppal::gpio::{Gpio, OutputPin};
use rppal::spi::{Bus, Mode, SlaveSelect, Spi};
use std::path::PathBuf;

mod flash;

/// Program a lattice FPGA with the provided synthesized design.
///
/// Documentation: https://www.latticesemi.com/view_document?document_id=46502
///
/// This assumes the following pins are connected:
///
/// SPI 0:
/// - MISO: GPIO 9
/// - MOSI: GPIO 10
/// - SCK: GPIO 11
///
/// GPIO:
/// - FPGA CS: GPIO 13
/// - Flash CS: GPIO 5
/// - FPGA Reset: GPIO 6
///
/// You may need to enable access to SPI and GPIO peripherals in the Pi's configuration, accessible
/// either through `raspi-config` or /boot/config.txt
#[derive(Parser)]
#[command(author, version, long_about, verbatim_doc_comment)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Program the FPGA's internal flash
    Sram {
        /// Path to the input RTL
        input: PathBuf,

        /// SPI baud rate
        ///
        /// Values that are too low or too high seem to corrupt the bitstream.
        #[arg(short, long, default_value = "10000000")]
        baud: u32,

        /// SPI transfer buffer size
        ///
        /// The maximum possible value is 65536, but any value above 4096 must be set in the Pi's
        /// boot configuration (by inserting spidev.bufsiz=<desired value> in /boot/cmdline.txt).
        #[arg(short, long, default_value = "16384")]
        transfer: usize,
    },
    /// Program the flash chip
    Flash {
        /// Path to the input RTL
        input: PathBuf,
    },
    /// Dump the flash
    Dump {
        /// The address to dump
        #[arg(short, long, default_value = "0")]
        address: usize,

        /// The amount of bytes to dump
        #[arg(short, long, default_value = "256")]
        length: usize,
    },
}

#[allow(dead_code)]
struct SramProgrammer {
    spi: Spi,
    fpga_reset: OutputPin,
    fpga_cs: OutputPin,
    flash_cs: OutputPin,
}

impl SramProgrammer {
    pub fn new(baud: u32) -> Result<Self> {
        let mut spi = Spi::new(Bus::Spi0, SlaveSelect::Ss0, baud, Mode::Mode0)
            .with_context(|| "Failed to acquire SPI")?;

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

        sleep(1);
        // Set CRESET_B low for at least 200 ns, ensuring the FPGA's CS is low when reset is
        // released
        fpga_reset.set_low();
        fpga_cs.set_low();
        sleep(1);
        // Wait for at least 1200 us as the FPGA clears configuration memory
        fpga_reset.set_high();
        sleep(10);

        // Set CS high and clock in 8 dummy bits
        fpga_cs.set_high();
        spi.write(&[0u8])?;
        fpga_cs.set_low();

        // Device ready for configuration
        Ok(Self {
            spi,
            fpga_reset,
            fpga_cs,
            flash_cs,
        })
    }

    pub fn program_bytes(mut self, mut data: Vec<u8>, transfer: usize) -> Result<()> {
        if transfer > 65536 {
            return Err(anyhow::Error::msg(format!(
                "SPI transfer buffer (set to {transfer}) must be less than 65536"
            )));
        }

        // The transaction requires 49 dummy bits after waiting a maximum of 100 clocks
        data.extend([0u8; 18]);
        let bar = indicatif::ProgressBar::new(data.len() as u64);
        bar.tick();

        for block in data.chunks(transfer) {
            self.spi
                .write(block)
                .with_context(|| "Error writing to SPI bus")?;
            bar.inc(block.len() as u64);
        }

        sleep(1);
        self.fpga_cs.set_high();
        sleep(1);

        Ok(())
    }

    pub fn reset() -> Result<()> {
        let gpio = Gpio::new().with_context(|| "Failed to acquire GPIO")?;

        gpio.get(6)?.into_input().set_reset_on_drop(false);
        gpio.get(13)?.into_input().set_reset_on_drop(false);
        gpio.get(5)?.into_input().set_reset_on_drop(false);

        Ok(())
    }
}

fn sleep(milliseconds: u64) {
    std::thread::sleep(std::time::Duration::from_millis(milliseconds));
}

fn program(filepath: PathBuf, baud: u32, transfer: usize) -> Result<()> {
    let data = std::fs::read(filepath).with_context(|| "Error reading input file")?;
    let programmer = SramProgrammer::new(baud)?;
    programmer.program_bytes(data, transfer)?;

    Ok(())
}

fn flash(filepath: PathBuf) -> Result<()> {
    let data = std::fs::read(filepath).with_context(|| "Error reading input file")?;
    let mut programmer = FlashProgrammer::new()?;
    println!("Flashing data...");
    programmer.flash_data(&data, 0)?;
    println!("Verifying data...");
    programmer.verify_data(&data, 0)?;

    Ok(())
}

fn dump(address: usize, length: usize) -> Result<Vec<u8>> {
    let mut programmer = FlashProgrammer::new()?;

    Ok(programmer.read_arbitrary(address, length))
}

fn main() {
    let args = Cli::parse();
    use std::io::Write;

    let message = match args.command {
        Commands::Sram {
            input,
            baud,
            transfer,
        } => {
            let result = program(input, baud, transfer);
            let reset = SramProgrammer::reset();

            match (result, reset) {
                (Ok(_), Ok(_)) => "Succesfully programmed device!".into(),
                (Err(e), Ok(_)) => format!("Failed to program device: {e}"),
                (Ok(_), Err(r)) => {
                    format!("Succesfully programmed device, but failed to reset: {r}")
                }
                (Err(e), Err(r)) => {
                    format!("Failed to program device: {e}\nAnd failed to reset: {r}")
                }
            }
        }
        Commands::Flash { input } => {
            FlashProgrammer::reset().expect("Error releasing pins");

            match flash(input) {
                Ok(_) => "Succesfully flashed device!".into(),
                Err(e) => format!("Failed to flash device: {e}"),
            }
        }
        Commands::Dump { address, length } => {
            FlashProgrammer::reset().expect("Error releasing pins");

            match dump(address, length) {
                Ok(data) => {
                    std::io::stdout().write_all(&data).unwrap();
                    return;
                }
                Err(e) => {
                    eprintln!("Error dumping data: {e}");
                    return;
                }
            }
        }
    };

    println!("{message}");
}
