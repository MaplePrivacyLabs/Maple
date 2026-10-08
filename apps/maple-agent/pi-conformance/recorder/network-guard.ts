/** Provider fixtures must supply an explicit local fetch implementation. */
let attempts = 0;
const rejectNetwork: typeof fetch = async () => {
  attempts++;
  throw new Error("The Pi reference recorder forbids network requests");
};

// Install before importing any Pi module: an incorrectly resolved SDK mock
// must fail here instead of sending even synthetic fixture data to a provider.
globalThis.fetch = rejectNetwork;

export function assertNoNetworkAttempts() {
  if (attempts !== 0) {
    throw new Error(`The Pi reference recorder attempted ${attempts} network requests`);
  }
}
