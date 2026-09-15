/**
 * Device-clock policy for Nitro attestation certificate validity.
 *
 * Every certificate in the attestation chain must contain the current device
 * time, as AWS specifies. The enclave leaf certificate is issued without any
 * backdating and re-issued roughly every 2 h 45 m, so a device clock that
 * runs a few seconds slow used to fail right after each re-issue because the
 * leaf looked not-yet-valid. `notBefore` therefore gets a small leeway;
 * `notAfter` stays strict so a fast device clock is still bounded by the
 * leaf's remaining validity (15 minutes to 3 hours).
 */
export const ATTESTATION_NOT_BEFORE_LEEWAY_MS = 5 * 60 * 1000;

/**
 * Device/enclave difference above which a validity failure is reported as a
 * device clock problem rather than a transient certificate problem.
 */
export const NOTICEABLE_CLOCK_SKEW_MS = 60 * 1000;

function describeDuration(ms: number): string {
  const abs = Math.abs(ms);
  const units: Array<[string, number]> = [
    ["day", 24 * 60 * 60 * 1000],
    ["hour", 60 * 60 * 1000],
    ["minute", 60 * 1000]
  ];
  for (const [name, size] of units) {
    if (abs >= size) {
      const count = Math.round(abs / size);
      return `${count} ${name}${count === 1 ? "" : "s"}`;
    }
  }
  return `${Math.round(abs / 1000)} seconds`;
}

export interface AttestationClockSkewDetails {
  /** The device clock reading used for the validity check. */
  deviceTime: Date;
  /** The `timestamp` the enclave signed into the attestation document. */
  enclaveTime: Date;
  /** Position of the failing certificate in the verified chain. */
  certificateIndex: number;
  notBefore: Date;
  notAfter: Date;
}

/**
 * A certificate in the verified attestation chain is outside its validity
 * window on this device's clock. `skewMs` compares the device clock with the
 * enclave's signed timestamp, so the message can tell the user whether their
 * date, time or time zone is wrong. This is a device-side problem, not a
 * credential or server problem, so callers should surface the message
 * instead of replacing it with a generic sign-in failure.
 */
export class AttestationClockSkewError extends Error {
  readonly deviceTime: Date;
  readonly enclaveTime: Date;
  /** Positive when the device clock is ahead of the enclave. */
  readonly skewMs: number;
  readonly certificateIndex: number;
  readonly notBefore: Date;
  readonly notAfter: Date;
  /** True when the device/enclave difference alone explains the failure. */
  readonly deviceClockLikelyWrong: boolean;

  constructor(details: AttestationClockSkewDetails) {
    const skewMs = details.deviceTime.getTime() - details.enclaveTime.getTime();
    const deviceClockLikelyWrong = Math.abs(skewMs) >= NOTICEABLE_CLOCK_SKEW_MS;
    const expired = details.deviceTime.getTime() > details.notAfter.getTime();
    const state = expired ? "expired" : "not yet valid";
    super(
      deviceClockLikelyWrong
        ? `This device's clock is about ${describeDuration(skewMs)} ${skewMs >= 0 ? "ahead of" : "behind"} ` +
            `the secure enclave (device: ${details.deviceTime.toISOString()}, ` +
            `enclave: ${details.enclaveTime.toISOString()}), so the enclave's certificate looks ${state}. ` +
            "Check the device's date, time and time zone settings, then try again."
        : `The secure enclave's certificate is ${state} on this device's clock ` +
            `(valid ${details.notBefore.toISOString()} to ${details.notAfter.toISOString()}, ` +
            `device: ${details.deviceTime.toISOString()}). Try again in a moment; if it keeps happening, ` +
            "check the device's date, time and time zone settings."
    );
    this.name = "AttestationClockSkewError";
    this.deviceTime = details.deviceTime;
    this.enclaveTime = details.enclaveTime;
    this.skewMs = skewMs;
    this.certificateIndex = details.certificateIndex;
    this.notBefore = details.notBefore;
    this.notAfter = details.notAfter;
    this.deviceClockLikelyWrong = deviceClockLikelyWrong;
  }
}

/** Whether `deviceTime` falls inside a certificate's validity window, with the notBefore leeway. */
export function isValidAtDeviceTime(notBefore: Date, notAfter: Date, deviceTime: Date): boolean {
  const time = deviceTime.getTime();
  return (
    notBefore.getTime() - ATTESTATION_NOT_BEFORE_LEEWAY_MS <= time && time <= notAfter.getTime()
  );
}
