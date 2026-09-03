import { X509Certificate, X509ChainBuilder } from "@peculiar/x509";
import { decode, encode } from "@stablelib/base64";
import * as cbor from "cbor2";
import { z } from "zod";
import { fetchAttestationDocument, getApiUrl } from "./api";
import awsRootCertDer from "../assets/aws_root.der";

// Assert that the root cert is not empty
if (!awsRootCertDer || awsRootCertDer.length === 0) {
  throw new Error("AWS root certificate is empty or not loaded correctly");
}

const AttestationDocumentSchema = z.object({
  module_id: z.string().min(1),
  digest: z.literal("SHA384"),
  timestamp: z.number().min(1677721600),
  pcrs: z.map(z.number(), z.instanceof(Uint8Array)),
  certificate: z.instanceof(Uint8Array),
  cabundle: z.array(z.instanceof(Uint8Array)),
  public_key: z.nullable(z.instanceof(Uint8Array)),
  user_data: z.nullable(z.instanceof(Uint8Array)),
  nonce: z.nullable(z.instanceof(Uint8Array))
});

export type AttestationDocument = z.infer<typeof AttestationDocumentSchema>;

const ParsedAttestationDocumentSchema = z.object({
  protected: z.instanceof(Uint8Array),
  // There's an "unprotected" header in the CBOR, but we never use it
  payload: z.instanceof(Uint8Array),
  signature: z.instanceof(Uint8Array)
});

type ParsedAttestationDocument = z.infer<typeof ParsedAttestationDocumentSchema>;

export async function parseDocumentData(
  attestationDocumentBase64: string
): Promise<ParsedAttestationDocument> {
  try {
    if (!attestationDocumentBase64) {
      throw new Error("Attestation document is empty.");
    }
    const attestationDocumentBuffer = decode(attestationDocumentBase64);
    const cborAttestationDocument: Uint8Array[] = cbor.decode(attestationDocumentBuffer);
    const protectedHeader = cborAttestationDocument[0];

    // There's an "unprotected" header in the CBOR, but we never use it
    // const unprotectedHeader = cborAttestationDocument[1];

    // The payload is another CBOR thing but we parse it later
    const payload = cborAttestationDocument[2];
    const signature = cborAttestationDocument[3];

    // Zod will make sure these are all Uint8Arrays
    // TODO: validate length?
    const zodParsed = ParsedAttestationDocumentSchema.parse({
      protected: protectedHeader,
      payload,
      signature
    });
    return zodParsed;
  } catch (error) {
    console.error("Error parsing document data:", error);
    throw new Error("Failed to parse document data.");
  }
}

// Parses the internal attestation document
export async function parseDocumentPayload(payload: Uint8Array): Promise<AttestationDocument> {
  try {
    const documentData = cbor.decode(payload);
    const zodParsed = AttestationDocumentSchema.parse(documentData);
    return zodParsed;
  } catch (error) {
    console.error("Error parsing document payload:", error);
    throw new Error("Failed to parse document payload.");
  }
}

// Creates and serializes a COSE Signature1 structure exactly matching the Rust implementation
export function createSigStructure(bodyProtected: Uint8Array, payload: Uint8Array): Uint8Array {
  // This exactly matches the Rust SigStructure::new_sign1 implementation
  const sig1 = [
    "Signature1", // Context string
    bodyProtected, // Protected headers
    new Uint8Array(0), // external_aad (empty ByteBuf in Rust)
    payload // payload
  ];

  // Encode to CBOR - equivalent to Rust's as_bytes()
  return cbor.encode(sig1);
}

// Creates a COSE_Sign1 thing and verifies its signature
async function verifySignature(
  documentData: ParsedAttestationDocument,
  publicKey: CryptoKey
): Promise<boolean> {
  try {
    console.log("SIGNATURE:");
    console.log(encode(documentData.signature));

    // Create signature bytes
    const signatureBytes = createSigStructure(documentData.protected, documentData.payload);

    // Hash the COSE_Sign1 with SHA-384 for debugging (crypto subtle will hash it as well);
    const hashBuffer = await crypto.subtle.digest("SHA-384", signatureBytes);

    console.log("SIGNATURE STRUCTURE DIGEST:");
    console.log(encode(new Uint8Array(hashBuffer)));

    // Verify the signature using Web Crypto API
    const verified = await crypto.subtle.verify(
      // TODO: these could be derived from the document, but we're hardcoding them for now
      {
        name: "ECDSA",
        hash: "SHA-384"
      },
      publicKey,
      documentData.signature,
      signatureBytes
    );

    return verified;
  } catch (error) {
    console.error("Error verifying signature:", error);
    if (error instanceof Error) {
      throw new Error(`Signature verification failed: ${error.message}`);
    } else {
      throw new Error(`Signature verification failed: ${error}`);
    }
  }
}

export async function authenticate(
  attestationDocumentBase64: string,
  trustedRootCert: Uint8Array,
  nonce: string
): Promise<AttestationDocument> {
  return authenticateWithNonceBytes(
    attestationDocumentBase64,
    trustedRootCert,
    new TextEncoder().encode(nonce)
  );
}

/**
 * Verifies a Nitro attestation document against a byte-exact nonce.
 *
 * Transport V2 challenges are arbitrary random bytes, while the original V1
 * entry point uses a textual UUID. Both paths share the complete certificate,
 * signature, timestamp, and nonce verification below.
 */
export async function authenticateBytes(
  attestationDocumentBase64: string,
  trustedRootCert: Uint8Array,
  nonce: Uint8Array
): Promise<AttestationDocument> {
  return authenticateWithNonceBytes(
    attestationDocumentBase64,
    trustedRootCert,
    new Uint8Array(nonce)
  );
}

async function authenticateWithNonceBytes(
  attestationDocumentBase64: string,
  trustedRootCert: Uint8Array,
  nonce: Uint8Array
): Promise<AttestationDocument> {
  try {
    // Following the steps here: https://docs.aws.amazon.com/enclaves/latest/user/verify-root.html
    // Step 1. Decode the CBOR object and map it to a COSE_Sign1 structure
    const parsedDocument = await parseDocumentData(attestationDocumentBase64);

    // Step 2. Extract the attestation document from the COSE_Sign1 structure
    const document = await parseDocumentPayload(parsedDocument.payload);

    // Step 2.5 Make sure the nonce matches
    if (!document.nonce) {
      throw new Error("Attestation document does not have a nonce.");
    }

    const documentNonce = document.nonce;
    if (
      nonce.length !== documentNonce.length ||
      nonce.some((byte, index) => byte !== documentNonce[index])
    ) {
      console.log("Nonce mismatch");
      throw new Error("Attestation document's nonce does not match the provided nonce.");
    }

    // Step 3. Verify the certificate's chain
    const certs = [];

    // Verify that the trusted root cert is the same as the first cert in the cabundle
    const firstCertBase64 = encode(document.cabundle[0]);
    if (firstCertBase64 !== encode(trustedRootCert)) {
      console.error("Root cert doesn't match first cert");
      console.log("First cert base64:", firstCertBase64);
      console.log("Trusted root cert base64:", encode(trustedRootCert));
      throw new Error("Root cert does not match first cert in attestation document.");
    }

    // Add the cabundle to the certs array... rust does this in reverse but it doesn't seem to matter
    for (let i = 0; i < document.cabundle.length; i++) {
      const cert = new X509Certificate(document.cabundle[i]);
      certs.push(cert);
    }

    // Add the document's cert as the "leaf" cert (the one that we're verifying)
    const leafCert = new X509Certificate(document.certificate);

    const chain = new X509ChainBuilder({
      certificates: certs
    });

    // `.build` checks each cert in the chain that it verifies with a pubkey of another cert in the chain
    // and only adds the ones that pass. If there's a failure there will be a length mismatch.
    // https://github.com/PeculiarVentures/x509/blob/1158bcbe2c5393196d66b9096b7b5e9c35901bce/src/x509_chain_builder.ts#L61
    const certChainItems = await chain.build(leafCert);
    console.log("Chain items:", certChainItems);

    // The chain builder checks signatures but not expiration
    // So let's check for expiration ourselves
    const date = new Date();
    const time = date.getTime();
    for (let i = 0; i < certChainItems.length; i++) {
      const cert = certChainItems[i];
      console.log("CERT: ", i);
      console.log(cert.subject);
      console.log("Not before:", cert.notBefore);
      console.log("Not after:", cert.notAfter);
      console.log(cert.toString("pem"));

      if (cert.notBefore.getTime() > time || cert.notAfter.getTime() < time) {
        throw new Error("Certificate is expired.");
      } else {
        console.log(`Certificate ${i} is not expired.`);
      }
    }

    // The chain should have the whole cabundle plus the leaf cert
    if (certChainItems.length !== document.cabundle.length + 1) {
      throw new Error("Certificate chain length does not match length of cabundle.");
    }

    // Step 4. Ensure the attestation document is properly signed

    // Get the webcrypto version of the public key (the raw bytes are the same fwiw)
    const publicKey = leafCert.publicKey;
    console.log("PUBLIC KEY:");
    console.log(encode(new Uint8Array(publicKey.rawData)));
    const pubEcKey = await publicKey.export();

    // Verify the signature!
    const verified = await verifySignature(parsedDocument, pubEcKey);

    console.log("Signature verified:", verified);

    if (!verified) {
      throw new Error("Signature verification failed.");
    }

    return document;
  } catch (error) {
    console.error("Error verifying attestation document:", error);
    throw error;
  }
}

// For localhost we get a fake document and we only need the public key
const FakeAttestationDocumentSchema = z.object({
  public_key: z.nullable(z.instanceof(Uint8Array))
});

type FakeAttestationDocument = z.infer<typeof FakeAttestationDocumentSchema>;

const LOCAL_DEVELOPMENT_API_HOSTS = new Set(["127.0.0.1", "localhost", "0.0.0.0", "[::1]"]);

export function isLocalDevelopmentApiUrl(apiUrl: string): boolean {
  try {
    const url = new URL(apiUrl);
    return url.protocol === "http:" && LOCAL_DEVELOPMENT_API_HOSTS.has(url.hostname.toLowerCase());
  } catch {
    return false;
  }
}

async function fakeAuthenticate(
  attestationDocumentBase64: string
): Promise<FakeAttestationDocument> {
  const attestationDocumentBuffer = decode(attestationDocumentBase64);
  const cborAttestationDocument: Uint8Array[] = cbor.decode(attestationDocumentBuffer);
  const payload = cborAttestationDocument[2];
  const payloadDecoded = cbor.decode(payload);
  const zodParsed = await FakeAttestationDocumentSchema.parse(payloadDecoded);
  return zodParsed;
}

export async function verifyAttestation(
  nonce: string,
  explicitApiUrl?: string
): Promise<AttestationDocument> {
  try {
    const attestationDocumentBase64 = await fetchAttestationDocument(nonce, explicitApiUrl);

    // Get the API URL from the API layer where it's already set
    // First check explicit URL, then check both possible APIs
    const apiUrl = explicitApiUrl || getApiUrl();

    // With a local backend we get a fake attestation document, so we'll just pretend to authenticate it
    if (apiUrl && isLocalDevelopmentApiUrl(apiUrl)) {
      console.log("DEV MODE: Using fake attestation document");
      const fakeDocument = await fakeAuthenticate(attestationDocumentBase64);
      return fakeDocument as AttestationDocument;
    }

    // The real thing!
    const verifiedDocument = await authenticate(attestationDocumentBase64, awsRootCertDer, nonce);
    return verifiedDocument;
  } catch (error) {
    if (error instanceof Error) {
      console.error("Error verifying attestation document:", error);
      throw new Error(`Couldn't process attestation document: ${error.message}`);
    } else {
      console.error("Error verifying attestation document:", error);
      throw new Error("Couldn't process attestation document.");
    }
  }
}
