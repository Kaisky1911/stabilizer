#![deny(warnings)]
#![no_std]
#![no_main]
#![cfg_attr(feature = "nightly", feature(core_intrinsics))]

use stm32h7xx_hal as hal;

use rtic::cyccnt::{Instant, U32Ext};

use stabilizer::{hardware, ADC_SAMPLE_TICKS_LOG2, SAMPLE_BUFFER_SIZE_LOG2};

use dsp::{iir, iir_int, lockin::Lockin, rpll::RPLL};
use hardware::{
    Adc0Input, Adc1Input, Dac0Output, Dac1Output, InputStamper, AFE0, AFE1,
};

const SCALE: f32 = ((1 << 15) - 1) as f32;

// The number of cascaded IIR biquads per channel. Select 1 or 2!
const IIR_CASCADE_LENGTH: usize = 1;

#[rtic::app(device = stm32h7xx_hal::stm32, peripherals = true, monotonic = rtic::cyccnt::CYCCNT)]
const APP: () = {
    struct Resources {
        afes: (AFE0, AFE1),
        adcs: (Adc0Input, Adc1Input),
        dacs: (Dac0Output, Dac1Output),
        stack: hardware::NetworkStack,

        // Format: iir_state[ch][cascade-no][coeff]
        #[init([[[0.; 5]; IIR_CASCADE_LENGTH]; 2])]
        iir_state: [[iir::IIRState; IIR_CASCADE_LENGTH]; 2],
        #[init([[iir::IIR { ba: [1., 0., 0., 0., 0.], y_offset: 0., y_min: -SCALE - 1., y_max: SCALE }; IIR_CASCADE_LENGTH]; 2])]
        iir_ch: [[iir::IIR; IIR_CASCADE_LENGTH]; 2],

        timestamper: InputStamper,
        pll: RPLL,
        lockin: Lockin,
    }

    #[init]
    fn init(c: init::Context) -> init::LateResources {
        // Configure the microcontroller
        let (mut stabilizer, _pounder) = hardware::setup(c.core, c.device);

        let pll = RPLL::new(ADC_SAMPLE_TICKS_LOG2 + SAMPLE_BUFFER_SIZE_LOG2, 0);

        let lockin = Lockin::new(
            &iir_int::IIRState::lowpass(1e-3, 0.707, 2.), // TODO: expose
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

        init::LateResources {
            afes: stabilizer.afes,
            adcs: stabilizer.adcs,
            dacs: stabilizer.dacs,
            stack: stabilizer.net.stack,
            timestamper: stabilizer.timestamper,

            pll,
            lockin,
        }
    }

    /// Main DSP processing routine for Stabilizer.
    ///
    /// # Note
    /// Processing time for the DSP application code is bounded by the following constraints:
    ///
    /// DSP application code starts after the ADC has generated a batch of samples and must be
    /// completed by the time the next batch of ADC samples has been acquired (plus the FIFO buffer
    /// time). If this constraint is not met, firmware will panic due to an ADC input overrun.
    ///
    /// The DSP application code must also fill out the next DAC output buffer in time such that the
    /// DAC can switch to it when it has completed the current buffer. If this constraint is not met
    /// it's possible that old DAC codes will be generated on the output and the output samples will
    /// be delayed by 1 batch.
    ///
    /// Because the ADC and DAC operate at the same rate, these two constraints actually implement
    /// the same time bounds, meeting one also means the other is also met.
    ///
    /// TODO: document lockin
    #[task(binds=DMA1_STR4, resources=[adcs, dacs, iir_state, iir_ch, lockin, timestamper, pll], priority=2)]
    fn process(c: process::Context) {
        let adc_samples = [
            c.resources.adcs.0.acquire_buffer(),
            c.resources.adcs.1.acquire_buffer(),
        ];

        let dac_samples = [
            c.resources.dacs.0.acquire_buffer(),
            c.resources.dacs.1.acquire_buffer(),
        ];

        let iir_ch = c.resources.iir_ch;
        let iir_state = c.resources.iir_state;
        let lockin = c.resources.lockin;

        let (pll_phase, pll_frequency) = c.resources.pll.update(
            c.resources.timestamper.latest_timestamp().map(|t| t as i32),
            22, // relative PLL frequency bandwidth: 2**-22, TODO: expose
            22, // relative PLL phase bandwidth: 2**-22, TODO: expose
        );

        // Harmonic index of the LO: -1 to _de_modulate the fundamental
        let harmonic: i32 = -1;
        // Demodulation LO phase offset
        let phase_offset: i32 = 0;
        let sample_frequency =
            (pll_frequency >> SAMPLE_BUFFER_SIZE_LOG2).wrapping_mul(harmonic);
        let mut sample_phase =
            phase_offset.wrapping_add(pll_phase.wrapping_mul(harmonic));

        for i in 0..adc_samples[0].len() {
            // Convert to signed, MSB align the ADC sample.
            let input = (adc_samples[0][i] as i16 as i32) << 16;
            // Obtain demodulated, filtered IQ sample.
            let output = lockin.update(input, sample_phase);
            // Advance the sample phase.
            sample_phase = sample_phase.wrapping_add(sample_frequency);

            // Convert from IQ to power and phase.
            let mut power = output.power() as _;
            let mut phase = output.phase() as _;

            // Filter power and phase through IIR filters.
            // Note: Normalization to be done in filters. Phase will wrap happily.
            for j in 0..iir_state[0].len() {
                power = iir_ch[0][j].update(&mut iir_state[0][j], power);
                phase = iir_ch[1][j].update(&mut iir_state[1][j], phase);
            }

            // Note(unsafe): range clipping to i16 is ensured by IIR filters above.
            // Convert to DAC data.
            unsafe {
                dac_samples[0][i] =
                    power.to_int_unchecked::<i16>() as u16 ^ 0x8000;
                dac_samples[1][i] =
                    phase.to_int_unchecked::<i16>() as u16 ^ 0x8000;
            }
        }
    }

    #[idle(resources=[stack, iir_state, iir_ch, afes])]
    fn idle(c: idle::Context) -> ! {
        let mut time = 0u32;
        let mut next_ms = Instant::now();

        // TODO: Replace with reference to CPU clock from CCDR.
        next_ms += 400_000.cycles();

        loop {
            let tick = Instant::now() > next_ms;

            if tick {
                next_ms += 400_000.cycles();
                time += 1;
            }

            let sleep = !c.resources.stack.poll(time);

            if sleep {
                cortex_m::asm::wfi();
            }
        }
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
