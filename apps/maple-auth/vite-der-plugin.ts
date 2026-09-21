import fs from "fs";

export default function derPlugin() {
  return {
    name: "vite-der-plugin",
    transform(_src: string, id: string) {
      if (id.endsWith(".der")) {
        // Convert the source to a Uint8Array
        // Read the .der file as a buffer
        const buffer = fs.readFileSync(id);

        // Convert the buffer to a Uint8Array
        const uint8Array = new Uint8Array(buffer);

        // Generate code to create and export a Uint8Array
        return {
          code: `export default new Uint8Array([${uint8Array.toString()}]);`,
          map: null
        };
      }
    }
  };
}
