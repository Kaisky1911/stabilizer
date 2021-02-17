#![deny(warnings)]
#![no_std]
#![no_main]

use stm32h7xx_hal as hal;

use miniconf::{
    embedded_nal::{IpAddr, Ipv4Addr},
    minimq, MqttInterface, StringSet,
};

use serde::Deserialize;
use stabilizer::{hardware, hardware::design_parameters};

use dsp::{lockin::Lockin, rpll::RPLL, Accu};

use hardware::{
    Adc0Input, Adc1Input, Dac0Output, Dac1Output, InputStamper, AFE0, AFE1,
};

#[derive(Deserialize, StringSet)]
pub struct Settings {
    data: u32,
}

impl Settings {
    pub fn new() -> Self {
        Self { data: 5 }
    }
}

#[rtic::app(device = stm32h7xx_hal::stm32, peripherals = true, monotonic = rtic::cyccnt::CYCCNT)]
const APP: () = {
    struct Resources {
        afes: (AFE0, AFE1),
        adcs: (Adc0Input, Adc1Input),
        dacs: (Dac0Output, Dac1Output),
        clock: hardware::CycleCounter,
        mqtt_interface: MqttInterface<
            Settings,
            hardware::NetworkStack,
            minimq::consts::U256,
        >,

        timestamper: InputStamper,
        pll: RPLL,
        lockin: Lockin,
    }

    #[init]
    fn init(c: init::Context) -> init::LateResources {
        // Configure the microcontroller
        let (mut stabilizer, _pounder) = hardware::setup(c.core, c.device);

        let mqtt_interface = {
            let mqtt_client = {
                let broker = IpAddr::V4(Ipv4Addr::new(10, 34, 16, 1));
                minimq::MqttClient::new(
                    broker,
                    "stabilizer",
                    stabilizer.net.stack,
                )
                .unwrap()
            };

            MqttInterface::new(mqtt_client, "stabilizer", Settings::new())
                .unwrap()
        };

        let pll = RPLL::new(
            design_parameters::ADC_SAMPLE_TICKS_LOG2
                + design_parameters::SAMPLE_BUFFER_SIZE_LOG2,
        );

        // Enable ADC/DAC events
        stabilizer.adcs.0.start();
        stabilizer.adcs.1.start();
        stabilizer.dacs.0.start();
        stabilizer.dacs.1.start();

        // Start recording digital input timestamps.
        stabilizer.timestamp_timer.start();

        // Start sampling ADCs.
        stabilizer.adc_dac_timer.start();

        // Enable the timestamper.
        stabilizer.timestamper.start();

        init::LateResources {
            mqtt_interface,
            afes: stabilizer.afes,
            adcs: stabilizer.adcs,
            dacs: stabilizer.dacs,
            timestamper: stabilizer.timestamper,
            clock: stabilizer.cycle_counter,

            pll,
            lockin: Lockin::default(),
        }
    }

    /// Main DSP processing routine.
    ///
    /// See `dual-iir` for general notes on processing time and timing.
    ///
    /// This is an implementation of a externally (DI0) referenced PLL lockin on the ADC0 signal.
    /// It outputs either I/Q or power/phase on DAC0/DAC1. Data is normalized to full scale.
    /// PLL bandwidth, filter bandwidth, slope, and x/y or power/phase post-filters are available.
    #[task(binds=DMA1_STR4, resources=[adcs, dacs, lockin, timestamper, pll], priority=2)]
    fn process(c: process::Context) {
        let adc_samples = [
            c.resources.adcs.0.acquire_buffer(),
            c.resources.adcs.1.acquire_buffer(),
        ];

        let dac_samples = [
            c.resources.dacs.0.acquire_buffer(),
            c.resources.dacs.1.acquire_buffer(),
        ];

        let lockin = c.resources.lockin;

        let timestamp = c
            .resources
            .timestamper
            .latest_timestamp()
            .unwrap_or(None) // Ignore data from timer capture overflows.
            .map(|t| t as i32);
        let (pll_phase, pll_frequency) = c.resources.pll.update(
            timestamp,
            21, // frequency settling time (log2 counter cycles), TODO: expose
            21, // phase settling time, TODO: expose
        );

        // Harmonic index of the LO: -1 to _de_modulate the fundamental (complex conjugate)
        let harmonic: i32 = -1; // TODO: expose

        // Demodulation LO phase offset
        let phase_offset: i32 = 0; // TODO: expose

        // Log2 lowpass time constant
        let time_constant: u8 = 6; // TODO: expose

        let sample_frequency = ((pll_frequency
            // half-up rounding bias
            // .wrapping_add(1 << design_parameters::SAMPLE_BUFFER_SIZE_LOG2 - 1)
            >> design_parameters::SAMPLE_BUFFER_SIZE_LOG2)
            as i32)
            .wrapping_mul(harmonic);
        let sample_phase =
            phase_offset.wrapping_add(pll_phase.wrapping_mul(harmonic));

        let output = adc_samples[0]
            .iter()
            .zip(Accu::new(sample_phase, sample_frequency))
            // Convert to signed, MSB align the ADC sample.
            .map(|(&sample, phase)| {
                lockin.update(sample as i16, phase, time_constant)
            })
            .last()
            .unwrap();

        let conf = "frequency_discriminator";
        let output = match conf {
            // Convert from IQ to power and phase.
            "power_phase" => [(output.log2() << 24) as _, output.arg()],
            "frequency_discriminator" => [pll_frequency as _, output.arg()],
            _ => [output.0, output.1],
        };

        // Convert to DAC data.
        for i in 0..dac_samples[0].len() {
            dac_samples[0][i] = (output[0] >> 16) as u16 ^ 0x8000;
            dac_samples[1][i] = (output[1] >> 16) as u16 ^ 0x8000;
        }
    }

    #[idle(resources=[mqtt_interface, clock], spawn=[settings_update])]
    fn idle(mut c: idle::Context) -> ! {
        let clock = c.resources.clock;
        loop {
            let sleep = c.resources.mqtt_interface.lock(|interface| {
                !interface.network_stack().poll(clock.current_ms())
            });

            match c
                .resources
                .mqtt_interface
                .lock(|interface| interface.update().unwrap())
            {
                miniconf::Action::Continue => {
                    if sleep {
                        cortex_m::asm::wfi();
                    }
                }
                miniconf::Action::CommitSettings => {
                    c.spawn.settings_update().unwrap()
                }
            }
        }
    }

    #[task(priority = 1, resources=[mqtt_interface, afes])]
    fn settings_update(c: settings_update::Context) {
        let _settings = &c.resources.mqtt_interface.settings;
        //c.resources.iir_ch.lock(|iir| *iir = settings.iir);
        // TODO: Update AFEs
    }

    #[task(binds = ETH, priority = 1)]
    fn eth(_: eth::Context) {
        unsafe { hal::ethernet::interrupt_handler() }
    }

    #[task(binds = SPI2, priority = 3)]
    fn spi2(_: spi2::Context) {
        panic!("ADC0 input overrun");
    }

    #[task(binds = SPI3, priority = 3)]
    fn spi3(_: spi3::Context) {
        panic!("ADC0 input overrun");
    }

    #[task(binds = SPI4, priority = 3)]
    fn spi4(_: spi4::Context) {
        panic!("DAC0 output error");
    }

    #[task(binds = SPI5, priority = 3)]
    fn spi5(_: spi5::Context) {
        panic!("DAC1 output error");
    }

    extern "C" {
        // hw interrupt handlers for RTIC to use for scheduling tasks
        // one per priority
        fn DCMI();
        fn JPEG();
        fn SDMMC();
    }
};
