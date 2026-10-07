# Performance check

A quick sanity check on a release build, not a gate. Run it on the same
machine before and after a change to the Agent's runtime, and compare.

1. Build the release binary and start it with startup logging:

   ```sh
   just release
   RUST_LOG=warn,maple_agent=debug target/release/maple-agent 2>startup.log &
   pid=$!
   ```

   Once the window is open, `grep 'startup: window open' startup.log` gives
   the time from process start to the window.

2. Ask for a long streamed answer, such as a 1500-word essay, and note
   whether the transcript scrolls smoothly. Then read the memory in KB with
   `ps -o rss= -p "$pid"`.

| Date | Build | Machine | Window open | Memory after a long answer | Streaming |
|---|---|---|---|---|---|
| 2026-10-07 | `93a235d2` | Apple M3 Max virtual machine, macOS 27.0.1 | 729 ms | 187 MB | Smooth |
