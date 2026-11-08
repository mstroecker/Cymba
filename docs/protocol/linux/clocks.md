# Clocks across a suspend

Which of the clocks a Rust program on Linux reads go on while the machine sleeps. From
the documentation and source named with each fact; not measured here.

The app acts on these in `podstate` and `nearby`:
[app/state.md](../../app/state.md#the-wall-clock),
[app/nearby.md](../../app/nearby.md#the-notification).

- **`CLOCK_MONOTONIC` does not count the time the system is suspended;**
  `CLOCK_BOOTTIME` is the same clock, but counts it (`clock_gettime(2)`, man-pages 6.19).
- **Rust's `std::time::Instant` reads `CLOCK_MONOTONIC` on Unix** other than Darwin.
  Its documentation says it is not specified whether a suspend counts as elapsed time,
  and that this varies across platforms and Rust versions (Rust 1.97.1,
  `std::time::Instant`, "Underlying System calls").
- **tokio's clock is `std::time::Instant`** unless paused for a test (tokio 1.54.1,
  `src/time/clock.rs`). So on Linux a tokio sleep does not count the time suspended, and
  wakes that much later on the wall clock.
- **The wall clock, `CLOCK_REALTIME`, measures real time and can be set**
  (`clock_gettime(2)`, man-pages 6.19); Rust's `SystemTime` reads it on Unix (Rust
  1.97.1, `std::time::SystemTime`, "Platform-specific behavior").

## Open questions

- None of this has been timed here: how much later than the wall clock a tokio sleep
  wakes across a suspend.
