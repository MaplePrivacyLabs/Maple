import type { TransportV2Header } from "./transportV2/protocol";

/** Self-reported application identity for usage reporting, never authorization. */
export type ClientApp = "chat" | "proxy" | "work";

export const CLIENT_APP_HEADER = "x-maple-client-app";

/** @internal Validate JavaScript callers as well as the public TypeScript type. */
export function snapshotClientApp(clientApp: ClientApp | undefined): ClientApp | undefined {
  if (
    clientApp !== undefined &&
    clientApp !== "chat" &&
    clientApp !== "proxy" &&
    clientApp !== "work"
  ) {
    throw new Error("clientApp must be chat, proxy, work, or undefined.");
  }
  return clientApp;
}

/** @internal Add the configured host identity inside the encrypted logical headers. */
export function withClientAppHeader(
  headers: TransportV2Header[] | undefined,
  clientApp: ClientApp | undefined
): TransportV2Header[] | undefined {
  if (clientApp === undefined) return headers;
  return [
    ...(headers ?? []).filter(({ name }) => name.toLowerCase() !== CLIENT_APP_HEADER),
    { name: CLIENT_APP_HEADER, value: clientApp }
  ];
}
