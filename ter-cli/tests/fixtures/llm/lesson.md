# Heartbeat

Modify the blink into a heartbeat of two short flashes followed by a pause.

Two gaps: the pattern and its timing.

The lesson's program toggled the LED every 500 ms; this is the same program
with a heartbeat instead of an even blink.

## The change

Two short flashes, then a pause, forever: on 100 ms, off 100 ms, on 100 ms,
off 700 ms. Same chip, same pin (GPIO3), same busy-wait on the
system timer.

## Gaps

- The four-step pattern: each step a level and a duration.
- The loop that walks the pattern and drives the pin. `toggle()` will not do
  it alone, since two of the four steps are both "off".

Not checked by the build: whether the rhythm on the board actually matches the
pattern.

