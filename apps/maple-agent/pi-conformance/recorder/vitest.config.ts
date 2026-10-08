import { defineConfig, mergeConfig } from "vitest/config";
import upstream from "../../vitest.config.ts";

export default mergeConfig(upstream, defineConfig({
  test: {
    include: ["test/maple-recorder/record.test.ts"],
    fileParallelism: false,
    maxWorkers: 1,
    sequence: { shuffle: false },
  },
}));
