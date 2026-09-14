// Transport V2 is intentionally dormant: consumers inside this repository may
// exercise the engine directly, but the package entry point does not export it
// and the existing SDK continues to use Transport V1 until the coordinated
// cutover layer.
export { TransportV2Client } from "./client";
export type { TransportV2ClientDependencies, TransportV2ClientOptions } from "./client";
export {
  TransportV2ProtocolError,
  type TransportV2Credential,
  type TransportV2Header,
  type TransportV2Request
} from "./protocol";
export { TransportV2RemoteError, type TransportV2LogicalResponse } from "./session";
