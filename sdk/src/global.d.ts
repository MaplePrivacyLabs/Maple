// Declare .der files as Uint8Array
declare module "*.der" {
  const content: Uint8Array;
  export default content;
}

// Vite and Bun both import a file's exact text with the ?raw suffix.
declare module "*?raw" {
  const text: string;
  export default text;
}
