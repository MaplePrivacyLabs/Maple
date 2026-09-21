type BunPluginApi = {
  plugin: (options: {
    name: string;
    setup: (build: {
      onLoad: (
        options: { filter: RegExp },
        callback: (args: {
          path: string;
        }) => { contents: string; loader?: string } | Promise<{ contents: string; loader?: string }>
      ) => void;
    }) => void;
  }) => void;
  file: (path: string) => { arrayBuffer: () => Promise<ArrayBuffer> };
};

const bun = (globalThis as unknown as { Bun?: BunPluginApi }).Bun;

if (bun) {
  bun.plugin({
    name: "der-loader",
    setup(build) {
      build.onLoad({ filter: /\.der$/ }, async (args) => {
        const buffer = await bun.file(args.path).arrayBuffer();
        const bytes = new Uint8Array(buffer);

        return {
          contents: `export default new Uint8Array([${Array.from(bytes).join(",")}]);`,
          loader: "js"
        };
      });
    }
  });
}

export {};
