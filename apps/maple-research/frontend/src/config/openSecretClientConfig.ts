import { openSecretPcrEnvironment } from "./openSecretPcrEnvironment";

const DEFAULT_OPEN_SECRET_CLIENT_ID = "ba5a14b5-d915-47b1-b7b1-afda52bc5fc6";

const PCR0_VALUES = [
  // Approved 2026-10-11 (services/opensecret signed history)
  "21d27bd19cdd02632485a6e8874753e42cf7f098f1335bb62a9c23e7f64da9100c02dba582da17ac68052fa6be16b7a9",
  // Approved 2026-10-09 (services/opensecret signed history; rollback)
  "643b3c293a4eac11ec14ab405d2405e46cb714263a12da6c62f485a0fa395cb3c3d5ef07b18d39a1f9715f25e7943b66"
];

const PCR0_DEV_VALUES = [
  // Approved 2026-10-11 (services/opensecret signed history)
  "a95956c3fb9457e509be6d614ffefcddf98fe594f77bb6a56891fc1f5bdec41da2add13db22e0908363c8d0929c04def",
  // Approved 2026-10-09 (services/opensecret signed history; rollback)
  "e573a3ddc8a27f387e24cfb02a8fa594fe782674de6ee1fa9385126b9f8ec38f4a236e91c371bdd2b9444623c1c1cc34"
];

export function openSecretClientConfig() {
  return {
    apiUrl: import.meta.env.VITE_OPEN_SECRET_API_URL,
    clientId: import.meta.env.VITE_CLIENT_ID || DEFAULT_OPEN_SECRET_CLIENT_ID,
    clientApp: "chat" as const,
    pcrConfig: {
      environment: openSecretPcrEnvironment(),
      pcr0Values: [...PCR0_VALUES],
      pcr0DevValues: [...PCR0_DEV_VALUES]
    }
  };
}
