//! Exercise: turn the lesson's blink on GPIO3 into a heartbeat.
//!
//! Two `todo!()`s to fill in. `cargo build` is the check — no board required.

#![no_std]
#![no_main]
// A compile check can only prove the code compiles, never that the LED blinks.
// Denying these closes the obvious hole: drop a `todo!()` without replacing it
// and `led` goes unused, which is now an error rather than a warning you can
// scroll past.
#![deny(unused_variables, unused_mut)]

use esp_backtrace as _;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::main;
use esp_hal::time::{Duration, Instant};

// The ESP-IDF bootloader will not start an image that carries no app
// descriptor, so every esp-hal binary needs this line. It is not part of the
// exercise — the generated project puts it here and it stays.
esp_bootloader_esp_idf::esp_app_desc!();

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());

    // Unchanged from the lesson: the pin is yours, nothing else can touch it.
    let mut led = Output::new(
        peripherals.GPIO3,
        Level::Low,
        OutputConfig::default(),
    );

    // TODO 1: describe the heartbeat — on 100 ms, off 100 ms, on 100 ms,
    // off 700 ms.
    //
    // You can write it as eight statements, but consider a table instead: one
    // entry per step, each a level and how long to hold it.
    //
    //   let pattern = [(Level::High, 100_u64), /* ... */];
    //
    // The shape you pick is the exercise. A table makes "add a third flash" an
    // edit to data; straight-line code makes it an edit to control flow.
    let pattern = [(Level::High, 100), (Level::Low, 100), (Level::High, 100), (Level::Low, 700)];

    // TODO 2: run the pattern, forever.
    //
    // Same waiting mechanism as the lesson — esp-hal 1.x keeps `Delay` behind
    // its `unstable` feature, so a step is a busy-loop on the system timer:
    //
    //   let start = Instant::now();
    //   while start.elapsed() < Duration::from_millis(millis) {}
    //
    // `set_level` puts the LED in a state you name; `toggle` cannot express
    // this pattern, because two of the four steps do not alternate.
    //
    // `main` returns `!`, so the outer loop must never break.
    loop {
        for step in pattern {
            led.set_level(step.0);
            let start = Instant::now();
            while start.elapsed() < step.1 {}
        }
    }
}
