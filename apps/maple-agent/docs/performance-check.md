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

3. To put numbers on "smoothly", start any build with `MAPLE_FRAME_STATS=1`.
   gpui then draws the time each frame took to build and paint in a corner
   of the main window; `MAPLE_FRAME_STATS=full` adds percentiles over the
   last 1000 frames. A frame has 16.7 ms at 60 Hz and 8.3 ms at 120 Hz.
   Compare the same interaction before and after a change: a long answer
   streaming, a hover sweep down the sidebar, the composer expanding.

| Date | Build | Machine | Window open | Memory after a long answer | Streaming |
|---|---|---|---|---|---|
| 2026-10-07 | `93a235d2` | Apple M3 Max virtual machine, macOS 27.0.1 | 729 ms | 187 MB | Smooth |
