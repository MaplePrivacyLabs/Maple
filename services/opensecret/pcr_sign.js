#!/usr/bin/env node

const fs = require('fs');
const crypto = require('crypto');

/**
 * Generates a P-384 (secp384r1) ECDSA keypair.
 * @returns {Object} An object containing the keypair information
 */
function generateKeypair() {
  // Generate an EC key pair on the P-384 curve
  const { publicKey, privateKey } = crypto.generateKeyPairSync('ec', {
    namedCurve: 'secp384r1',
  });

  // Export the private key in PKCS#8 DER format
  const privateKeyDer = privateKey.export({
    type: 'pkcs8',
    format: 'der',
  });

  // Export the public key in SPKI DER format
  const publicKeyDer = publicKey.export({
    type: 'spki',
    format: 'der',
  });

  // Base64-encode them for easy storage
  const privateKeyBase64 = privateKeyDer.toString('base64');
  const publicKeyBase64 = publicKeyDer.toString('base64');

  // Create PEM format for better human readability
  const privatePem = [
    '-----BEGIN PRIVATE KEY-----',
    ...privateKeyBase64.match(/.{1,64}/g) || [],
    '-----END PRIVATE KEY-----',
  ].join('\n');

  const publicPem = [
    '-----BEGIN PUBLIC KEY-----',
    ...publicKeyBase64.match(/.{1,64}/g) || [],
    '-----END PUBLIC KEY-----',
  ].join('\n');

  return {
    privateKeyBase64,
    publicKeyBase64,
    privatePem,
    publicPem,
    privateKey,
    publicKey
  };
}

/**
 * Signs data using ECDSA+SHA384 with P1363 (raw) encoding
 * @param {Buffer|string} privateKeyData - Private key in base64 DER format
 * @param {string} dataToSign - Data to sign
 * @returns {string} Base64-encoded signature
 */
function signData(privateKeyData, dataToSign) {
  // If privateKeyData is a string, assume it's base64 and convert to Buffer
  const privateKeyBuffer = Buffer.isBuffer(privateKeyData) 
    ? privateKeyData
    : Buffer.from(privateKeyData, 'base64');

  try {
    // Create the private key object
    const privateKey = crypto.createPrivateKey({
      key: privateKeyBuffer,
      format: 'der',
      type: 'pkcs8'
    });

    // Verify data is a string
    if (typeof dataToSign !== 'string') {
      throw new Error('Data to sign must be a string');
    }

    // Create the signer
    const signer = crypto.createSign('SHA384');
    signer.update(dataToSign);

    // Sign in "ieee-p1363" raw format which produces the raw r|s signature
    // This is directly compatible with Web Crypto API's verify method
    const signature = signer.sign({
      key: privateKey,
      dsaEncoding: 'ieee-p1363'
    });

    // Verify signature length (should be 96 bytes for P-384)
    if (signature.length !== 96) {
      console.warn(`Warning: Generated signature is ${signature.length} bytes (expected 96 for P-384)`);
    }

    return signature.toString('base64');
  } catch (error) {
    console.error("Error during signing: invalid key or signing input");
    throw error;
  }
}

/**
 * Main function to handle CLI commands
 */
function main() {
  const command = process.argv[2];

  if (!command) {
    console.log("Usage:");
    console.log("  pcr_sign.js generate-keys");
    console.log("  pcr_sign.js sign-pcr0 <pcr0-value>");
    process.exit(1);
  }

  if (command === "generate-keys") {
    const keys = generateKeypair();
    
    console.log("===== IMPORTANT: SAVE THESE KEYS SECURELY =====");
    console.log("Generate these keys once and securely store them for all future operations.");
    console.log("The private key is SENSITIVE and should be protected!\n");
    
    console.log("===== SET THESE ENVIRONMENT VARIABLES NOW =====");
    console.log(`export SIGNING_PRIVATE_KEY='${keys.privateKeyBase64}'`);
    console.log(`export SIGNING_PUBLIC_KEY='${keys.publicKeyBase64}'`);
    console.log("\nRun the above commands to set your environment variables.");
    console.log("Consider adding them to your .env file for persistence.\n");
    
    console.log("===== FOR FRONTEND INTEGRATION =====");
    console.log("// Add this constant to your frontend code for PCR signature verification");
    console.log(`const PCR_VERIFICATION_PUBLIC_KEY_B64 = "${keys.publicKeyBase64}";`);
    console.log("");
    
    console.log("===== KEY FORMATS (FOR REFERENCE) =====");
    console.log("PRIVATE KEY (PKCS#8 DER BASE64):");
    console.log(keys.privateKeyBase64);
    console.log("\nPUBLIC KEY (SPKI DER BASE64):");
    console.log(keys.publicKeyBase64);
    console.log("\nPRIVATE KEY (PEM):\n" + keys.privatePem);
    console.log("\nPUBLIC KEY (PEM):\n" + keys.publicPem);
    
    console.log("\n===== VERIFICATION COMMANDS =====");
    console.log("# Sign and append PCR measurements:");
    console.log("just append-pcr-dev");
    console.log("just append-pcr-prod");
    console.log("");
    console.log("# Verify PCR history signatures:");
    console.log("just verify-pcr-history dev");
    console.log("just verify-pcr-history prod");
    console.log("");
  }
  else if (command === "sign-pcr0") {
    // Get PCR0 value from argument or environment
    const pcr0 = process.argv[3];
    if (!pcr0 || !/^[0-9a-f]{96}$/.test(pcr0) || /^0+$/.test(pcr0)) {
      console.error("Error: PCR0 must be nonzero lowercase SHA-384 hexadecimal");
      console.log("Usage: pcr_sign.js sign-pcr0 <pcr0-value>");
      process.exit(1);
    }

    // Get private key from environment
    const privateKeyBase64 = process.env.SIGNING_PRIVATE_KEY;
    delete process.env.SIGNING_PRIVATE_KEY;
    if (!privateKeyBase64) {
      console.error("Error: SIGNING_PRIVATE_KEY environment variable is not set");
      console.error("Use the explicit SecretSpec signing recipe with the existing key.");
      process.exit(1);
    }

    try {
      // Sign just the PCR0 string
      const signature = signData(privateKeyBase64, pcr0);
      
      // Output only the signature (no comments or extra text)
      console.log(signature);
    } catch (error) {
      console.error("Error signing PCR0; check the existing signing-key configuration.");
      process.exit(1);
    }
  }
  else {
    console.log("Unknown command:", command);
    process.exit(1);
  }
}

// Run the main function if this script is executed directly
if (require.main === module) {
  main();
}

// Export functions for potential module usage
module.exports = {
  generateKeypair,
  signData
}; 