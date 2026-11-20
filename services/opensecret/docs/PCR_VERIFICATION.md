# PCR Verification System

This document describes the PCR (Platform Configuration Register) verification system used to validate Nitro Enclave measurements.

For Maple operations, follow the [signed PCR procedure](pcr-compatibility.md).
It preserves the existing verification key and publishes reviewed signed files
in the monorepo before a deployment needs them.
The examples below explain the format and an independent deployment's initial
setup; generating a new key is not part of routine Maple PCR updates. The SDKs
already implement verification, so clients should use their supported APIs.

## Overview

The system uses an append-only history of signed PCR measurements that allows the frontend to verify enclave measurements even when they're not in the default list. This provides:

1. **Transparency**: Full history of all deployed enclave measurements
2. **Security**: Each PCR measurement is cryptographically signed
3. **Auditability**: Changes to PCR values over time are tracked
4. **Flexibility**: New PCR values can be added without requiring frontend updates

## Signature Format

The system uses ECDSA with the P-384 curve and SHA-384 hash function:

- **Simplified approach**: Only the PCR0 string value is signed (not the entire JSON object)
- Signatures are created using ECDSA with the P-384 curve and SHA-384 hash
- The signature is produced in raw P1363 format (r and s concatenated in fixed 48-byte form)
- The signature is base64-encoded for storage in the history file
- No conversion is needed in the browser - the format is directly compatible with Web Crypto API

## Backend Commands

The following commands are available in the justfile:

### Generate Keys

```bash
./pcr_sign.js generate-keys
```

This generates an ECDSA key pair (using the P-384 curve) for signing PCR measurements and outputs them to the console:
- Private key in PKCS#8 DER format (base64-encoded)
- Public key in SPKI DER format (base64-encoded for Web Crypto API)
- Private key in PEM format (for human readability)
- Public key in PEM format (for human readability)

It also outputs ready-to-use commands for:
- Setting the SIGNING_PRIVATE_KEY and SIGNING_PUBLIC_KEY environment variables
- Verifying PCR history signatures

No files are created on disk - all keys are generated in memory and output to the console.

### Sign and Append PCR Measurements

```bash
# First set the environment variable with the base64-encoded private key
export SIGNING_PRIVATE_KEY='your-base64-encoded-private-key'

# Then run one of these commands
just append-pcr-dev     # For development PCRs
just append-pcr-prod    # For production PCRs
```

These commands:
1. Read the PCR measurements from the latest build
2. Sign the PCR0 value with your private key
3. Append all PCR values and the signature to the history file (if they don't already exist)

Duplicate PCR0 values are automatically detected and skipped to prevent redundancy.

### Verify PCR History

```bash
# First set the environment variable with the base64-encoded public key
export SIGNING_PUBLIC_KEY='your-base64-encoded-public-key'

# Then run one of these commands
just verify-pcr-history dev     # For development PCRs
just verify-pcr-history prod    # For production PCRs
```

This verifies all signatures in a PCR history file against the public key in the SIGNING_PUBLIC_KEY environment variable.

## Deployment Workflow

1. Generate keys:
   ```bash
   ./pcr_sign.js generate-keys
   ```

2. Set the environment variables for the private and public keys:
   ```bash
   export SIGNING_PRIVATE_KEY='base64-encoded-private-key-from-previous-step'
   export SIGNING_PUBLIC_KEY='base64-encoded-public-key-from-previous-step'
   ```

3. Build and append PCR values:
   ```bash
   just build-eif-dev  # Build the EIF for dev
   just append-pcr-dev # Sign and append to history
   ```

4. Commit and push `pcrDevHistory.json` to your repository:
   ```bash
   git add pcrDevHistory.json
   git commit -m "Update PCR history with latest measurements"
   git push
   ```

5. Deploy as usual:
   ```bash
   just deploy-dev-nix
   ```

## Frontend Integration

The frontend integration uses the Web Crypto API to verify PCR signatures in a simplified way by only verifying the PCR0 value.

### 1. Public Key Storage

Store the SPKI base64-encoded public key in your frontend code:

```javascript
// The public key in SPKI DER format, base64-encoded (from ./pcr_sign.js generate-keys)
const PCR_VERIFICATION_PUBLIC_KEY_B64 = "MHYwEAYHKoZIzj0CAQYFK4EEACIDYgAE...";
```

### 2. Import the Public Key

Create a function to import the public key into the Web Crypto API:

```javascript
async function importVerificationKey() {
  // Decode the base64 key to binary
  const binaryKey = Uint8Array.from(
    atob(PCR_VERIFICATION_PUBLIC_KEY_B64),
    c => c.charCodeAt(0)
  );
  
  // Import as SPKI format
  return await crypto.subtle.importKey(
    "spki",                  // The format: SubjectPublicKeyInfo
    binaryKey.buffer,        // The binary key data
    {
      name: "ECDSA",         // The algorithm
      namedCurve: "P-384"    // The curve (must be P-384 to match our backend)
    },
    false,                   // Not extractable
    ["verify"]               // Only for verification
  );
}
```

### 3. Fetch PCR History

Add a helper function to fetch the PCR history from your repository:

```javascript
async function fetchPcrHistory(env) {
  // Replace with your actual repository URL
  const baseUrl = "https://raw.githubusercontent.com/YourOrg/YourRepo/main";
  const url = env === 'dev' ? 
    `${baseUrl}/pcrDevHistory.json` : 
    `${baseUrl}/pcrProdHistory.json`;
  
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`Failed to fetch PCR history: ${response.status}`);
  }
  
  return await response.json();
}
```

### 4. Verify a Signature

This is the function for verifying a PCR0 signature:

```javascript
async function verifyPcr0Signature(pcr0, signatureBase64, publicKey) {
  try {
    // 1. Create the verifier with the correct hash algorithm
    const encoder = new TextEncoder();
    const pcr0Binary = encoder.encode(pcr0);
    
    // 2. Convert the base64 signature to binary
    const signatureBinary = Uint8Array.from(
      atob(signatureBase64),
      c => c.charCodeAt(0)
    );
    
    // 3. Verify using Web Crypto API
    return await crypto.subtle.verify(
      {
        name: "ECDSA",
        hash: { name: "SHA-384" }  // Must match the hash used for signing
      },
      publicKey,
      signatureBinary,
      pcr0Binary
    );
  } catch (error) {
    console.error("Signature verification error:", error);
    return false;
  }
}
```

### 5. PCR Validation Function

Create a function to check if a PCR is valid:

```javascript
async function validatePcr(pcr, env = 'prod') {
  try {
    // 1. Import the verification key
    const publicKey = await importVerificationKey();
    
    // 2. Fetch the PCR history
    const history = await fetchPcrHistory(env);
    
    // 3. Find a matching entry in the history
    for (const entry of history) {
      // Check if PCR values match
      if (entry.PCR0 === pcr.PCR0 &&
          entry.PCR1 === pcr.PCR1 &&
          entry.PCR2 === pcr.PCR2) {
        
        // 4. Verify the signature (only of PCR0)
        const isValid = await verifyPcr0Signature(entry.PCR0, entry.signature, publicKey);
        if (isValid) {
          return {
            valid: true,
            timestamp: entry.timestamp,
            message: `Verified PCR with signature from ${new Date(entry.timestamp * 1000).toLocaleString()}`
          };
        }
      }
    }
    
    // No match found
    return {
      valid: false,
      message: "PCR not found in verified history"
    };
  } catch (error) {
    console.error("PCR validation error:", error);
    return {
      valid: false,
      message: `Error validating PCR: ${error.message}`
    };
  }
}
```

### 6. Complete Example

Here's a complete example showing how to validate PCRs in a web application:

```javascript
// PCR Verification Module

// The public key from pcr_sign.js generate-keys output
const PCR_VERIFICATION_PUBLIC_KEY_B64 = "MHYwEAYHKoZIzj0CAQYFK4EEACIDYgAE...";

// Import the public key into Web Crypto API
async function importVerificationKey() {
  const binaryKey = Uint8Array.from(
    atob(PCR_VERIFICATION_PUBLIC_KEY_B64),
    c => c.charCodeAt(0)
  );
  
  return await crypto.subtle.importKey(
    "spki",
    binaryKey.buffer,
    {
      name: "ECDSA",
      namedCurve: "P-384"
    },
    false,
    ["verify"]
  );
}

// Fetch PCR history from repository
async function fetchPcrHistory(env) {
  const baseUrl = "https://raw.githubusercontent.com/YourOrg/YourRepo/main";
  const url = `${baseUrl}/pcr${env.charAt(0).toUpperCase() + env.slice(1)}History.json`;
  
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`Failed to fetch PCR history: ${response.status}`);
  }
  
  return await response.json();
}

// Verify a single PCR0 signature
async function verifyPcr0Signature(pcr0, signatureBase64, publicKey) {
  // Convert PCR0 string to binary
  const encoder = new TextEncoder();
  const pcr0Binary = encoder.encode(pcr0);
  
  // Convert signature from base64 to binary
  const signatureBinary = Uint8Array.from(
    atob(signatureBase64),
    c => c.charCodeAt(0)
  );
  
  // Verify
  try {
    return await crypto.subtle.verify(
      {
        name: "ECDSA",
        hash: { name: "SHA-384" }
      },
      publicKey,
      signatureBinary,
      pcr0Binary
    );
  } catch (error) {
    console.error("Verification error:", error);
    return false;
  }
}

// Main validation function
async function validatePcr(pcr, env = 'prod') {
  try {
    // Load key and history
    const [publicKey, history] = await Promise.all([
      importVerificationKey(),
      fetchPcrHistory(env)
    ]);
    
    // Check for matches
    for (const entry of history) {
      if (entry.PCR0 === pcr.PCR0 &&
          entry.PCR1 === pcr.PCR1 &&
          entry.PCR2 === pcr.PCR2) {
        
        const isValid = await verifyPcr0Signature(entry.PCR0, entry.signature, publicKey);
        if (isValid) {
          return {
            valid: true,
            timestamp: entry.timestamp,
            verifiedAt: new Date(entry.timestamp * 1000).toLocaleString()
          };
        }
      }
    }
    
    return { valid: false };
  } catch (error) {
    console.error("PCR validation failed:", error);
    return { valid: false, error: error.message };
  }
}

// Example usage:
async function checkPcr() {
  const pcr = {
    PCR0: "cc88f0edbccb5c92a46a2c4ba542c624123a793b002d1150153def94e34f3daa288f70162a8d163c5d36b31269624cb7", 
    PCR1: "e45de6f4e9809176f6adc68df999f87f32a602361247d5819d1edf11ac5a403cfbb609943705844251af85713a17c83a",
    PCR2: "7f3c7df92680edd708d19a25784d18883381cc34e16d3fe9079f7f117970ccb2eb4f403875f1340558f86a58edcdcea9"
  };
  
  const result = await validatePcr(pcr, 'dev');
  console.log("PCR valid?", result.valid);
  if (result.valid) {
    console.log("Verified at:", result.verifiedAt);
  }
}
```

### Key Advantages of this Approach

1. **Simplicity**: By signing only the PCR0 string value rather than a JSON object, we eliminate issues with JSON formatting, whitespace, and field ordering.

2. **Reliability**: The signature verification is much more reliable since it's based on a simple string rather than a complex JSON structure that could vary between signing and verification.

3. **Performance**: Signing and verifying a simple string is more efficient than working with JSON objects.

4. **Security**: The PCR0 value is the most important measurement that identifies the enclave. By signing it directly, we maintain the security guarantee while simplifying the implementation.

## Testing

During development, you can verify that signatures in a PCR history file are valid by running:

```bash
export SIGNING_PUBLIC_KEY='your-base64-encoded-public-key'
just verify-pcr-history dev
```

This will check all signatures in the history file and report any invalid entries.
