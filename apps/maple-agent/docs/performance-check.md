# Performance check

A short smoke and sanity check for a release build, not a gate. It covers
what the runtime plan asks for before the runtime swap: cold start to an
interactive window, memory at idle and after a long task, and whether a long
streamed answer stays smooth. Time to first token depends on the network, so
only Maple's own startup is timed. Record the numbers with the machine they
were taken on, and repeat them on the same machine before the swap merges.

## Recipe

1. Build the release binary from the component directory:

   ```sh
   just release
   ```

2. Run the script with the environment the app normally gets (the API
   origins and `XDG_*` roots of the account you test with), pointing it at
   the release binary. The second argument is the idle period in seconds:

   ```sh
   scripts/perf-check.sh target/release/maple-agent 30
   ```

   The script starts the app with `RUST_LOG=warn,maple_agent=debug`, waits
   for the `startup: runtime started` marker, prints every `startup:` marker
   with its millisecond offset from process start, samples resident memory
   when the window is open and again after the idle period, and leaves the
   app running.

3. In the running app, start a task and ask for a long streamed answer, for
   example a 1500-word essay. Watch the transcript while it streams and note
   whether it scrolls smoothly or stalls. When the answer ends, sample memory
   once more with the `ps` line the script prints, then quit the app.

The startup markers come from the app itself (`startup: logging ready`,
`settings loaded`, `backend ready`, `gpui app ready`, `window open`, `first
render`, `chat screen open`, `local bootstrap applied`, `credential
validation done`, `runtime started`). "Window open" is the number to watch
for cold start; "runtime started" is when the task list is live.

## Recorded runs

| Date | Build | Machine | Window open | Runtime started | Memory, window open | Memory after idle | Memory after long task | Long answer |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 93a235d2 (phase 0 trimmed build), `just release` | Apple M3 Max virtual machine, 8 cores, 48 GB, macOS 27.0.1 | 729 ms | 1956 ms | 113.7 MB | 114.0 MB | 186.8 MB | smooth: six samples 8-10 s apart over an 85 s stream each showed the transcript further along, following the tail; CPU 9-10 % |
