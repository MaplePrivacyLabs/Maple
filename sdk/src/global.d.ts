// Declare .der files as Uint8Array
declare module "*.der" {
  const content: Uint8Array;
  export default content;
}
