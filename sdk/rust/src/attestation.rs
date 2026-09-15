use crate::cbor::{self, Value as CborValue};
use crate::error::{Error, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use ring::signature;
use serde::{Deserialize, Serialize};
use x509_parser::prelude::*;

// AWS Nitro Root Certificate (production)
const AWS_NITRO_ROOT_CERT: &[u8] = include_bytes!("../assets/aws_nitro_root.der");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestationDocument {
    pub module_id: String,
    pub timestamp: u64,
    pub digest: String,
    pub pcrs: std::collections::HashMap<usize, Vec<u8>>,
    pub certificate: Vec<u8>,
    pub cabundle: Vec<Vec<u8>>,
    pub public_key: Option<Vec<u8>>,
    pub user_data: Option<Vec<u8>>,
    pub nonce: Option<Vec<u8>>,
}

/// Low-level AWS Nitro document verifier.
///
/// This verifies the certificate chain, document signature, and nonce. Nitro
/// authenticity alone does not identify an OpenSecret deployment. Production
/// callers should use `OpenSecretClient`, which additionally enforces its
/// configured `Pcr0TrustPolicy` before key exchange.
#[derive(Default)]
pub struct AttestationVerifier {
    expected_pcrs: Option<std::collections::HashMap<usize, Vec<u8>>>,
}

impl AttestationVerifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_expected_pcrs(mut self, pcrs: std::collections::HashMap<usize, Vec<u8>>) -> Self {
        self.expected_pcrs = Some(pcrs);
        self
    }

    pub fn verify_attestation_document(
        &self,
        document_b64: &str,
        expected_nonce: &str,
    ) -> Result<AttestationDocument> {
        self.verify_attestation_document_with(document_b64, |nonce_bytes| {
            let nonce_str = String::from_utf8(nonce_bytes.to_vec()).map_err(|e| {
                Error::AttestationVerificationFailed(format!("Invalid nonce encoding: {}", e))
            })?;

            if nonce_str != expected_nonce {
                return Err(Error::AttestationVerificationFailed(
                    "Nonce mismatch".to_string(),
                ));
            }
            Ok(())
        })
    }

    /// Verify a Nitro document against an exact binary nonce.
    ///
    /// Transport v2 uses a uniformly random 32-byte challenge. Keeping this
    /// entry point beside the established verifier ensures certificate and
    /// COSE validation remain identical without forcing binary freshness
    /// material through UTF-8. The caller still applies its PCR policy, and
    /// the public string API above is unchanged.
    pub(crate) fn verify_attestation_document_bytes(
        &self,
        document_b64: &str,
        expected_nonce: &[u8],
    ) -> Result<AttestationDocument> {
        self.verify_attestation_document_with(document_b64, |nonce_bytes| {
            if nonce_bytes != expected_nonce {
                return Err(Error::AttestationVerificationFailed(
                    "Nonce mismatch".to_string(),
                ));
            }
            Ok(())
        })
    }

    fn verify_attestation_document_with(
        &self,
        document_b64: &str,
        verify_nonce: impl FnOnce(&[u8]) -> Result<()>,
    ) -> Result<AttestationDocument> {
        self.verify_attestation_document_at(document_b64, verify_nonce, current_time_ms())
    }

    /// Verifies a document with an explicit device clock reading (milliseconds
    /// since the UNIX epoch). See [`ATTESTATION_NOT_BEFORE_LEEWAY_MS`].
    fn verify_attestation_document_at(
        &self,
        document_b64: &str,
        verify_nonce: impl FnOnce(&[u8]) -> Result<()>,
        device_now_ms: u64,
    ) -> Result<AttestationDocument> {
        let document_bytes = BASE64.decode(document_b64)?;

        // Parse COSE_Sign1 structure
        let cbor_value: CborValue = cbor::from_slice(&document_bytes)?;

        let cose_sign1 = match &cbor_value {
            CborValue::Array(arr) => arr,
            _ => {
                return Err(Error::AttestationVerificationFailed(
                    "Invalid COSE_Sign1 structure".to_string(),
                ))
            }
        };

        if cose_sign1.len() != 4 {
            return Err(Error::AttestationVerificationFailed(
                "COSE_Sign1 must have 4 elements".to_string(),
            ));
        }

        // Extract components
        let protected = match &cose_sign1[0] {
            CborValue::Bytes(b) => b,
            _ => {
                return Err(Error::AttestationVerificationFailed(
                    "Invalid protected header".to_string(),
                ))
            }
        };

        let payload = match &cose_sign1[2] {
            CborValue::Bytes(b) => b,
            _ => {
                return Err(Error::AttestationVerificationFailed(
                    "Invalid payload".to_string(),
                ))
            }
        };

        let signature = match &cose_sign1[3] {
            CborValue::Bytes(b) => b,
            _ => {
                return Err(Error::AttestationVerificationFailed(
                    "Invalid signature".to_string(),
                ))
            }
        };

        // Parse attestation document from payload
        let doc_cbor: CborValue = cbor::from_slice(payload)?;

        let doc = self.parse_attestation_document(&doc_cbor)?;

        // Verify freshness before accepting any attested key material.
        let nonce_bytes = doc.nonce.as_deref().ok_or_else(|| {
            Error::AttestationVerificationFailed(
                "Missing nonce in attestation document".to_string(),
            )
        })?;
        verify_nonce(nonce_bytes)?;

        // Verify certificate chain signatures up to the pinned AWS root
        self.verify_certificate_chain(&doc)?;

        // Verify the document signature; after this the payload, including
        // its `timestamp`, is authenticated by the Nitro PKI.
        self.verify_signature(protected, payload, signature, &doc)?;

        // Verify every certificate's validity window against the device clock,
        // as AWS specifies. This runs after the signature check so the signed
        // `timestamp` can explain a failure in terms of the device clock.
        self.verify_certificate_validity(&doc, device_now_ms)?;

        // Verify PCRs if expected
        if let Some(expected_pcrs) = &self.expected_pcrs {
            self.verify_pcrs(&doc, expected_pcrs)?;
        }

        Ok(doc)
    }

    fn parse_attestation_document(&self, cbor: &CborValue) -> Result<AttestationDocument> {
        let map = match cbor {
            CborValue::Map(m) => m,
            _ => {
                return Err(Error::AttestationVerificationFailed(
                    "Invalid attestation document format".to_string(),
                ))
            }
        };

        let mut doc = AttestationDocument {
            module_id: String::new(),
            timestamp: 0,
            digest: String::new(),
            pcrs: std::collections::HashMap::new(),
            certificate: Vec::new(),
            cabundle: Vec::new(),
            public_key: None,
            user_data: None,
            nonce: None,
        };

        for (key, value) in map {
            let key_str = match key {
                CborValue::Text(s) => s.as_str(),
                _ => {
                    return Err(Error::AttestationVerificationFailed(
                        "Invalid key in attestation document".to_string(),
                    ))
                }
            };

            match key_str {
                "module_id" => {
                    doc.module_id = match value {
                        CborValue::Text(s) => s.clone(),
                        _ => {
                            return Err(Error::AttestationVerificationFailed(
                                "Invalid module_id".to_string(),
                            ))
                        }
                    };
                }
                "timestamp" => {
                    doc.timestamp = match value {
                        CborValue::Integer(i) => cbor_integer_to_u64(*i, "timestamp")?,
                        _ => {
                            return Err(Error::AttestationVerificationFailed(
                                "Invalid timestamp: not an integer".to_string(),
                            ))
                        }
                    };
                }
                "digest" => {
                    doc.digest = match value {
                        CborValue::Text(s) => s.clone(),
                        _ => {
                            return Err(Error::AttestationVerificationFailed(
                                "Invalid digest".to_string(),
                            ))
                        }
                    };
                }
                "pcrs" => {
                    let pcrs_map = match value {
                        CborValue::Map(m) => m,
                        _ => {
                            return Err(Error::AttestationVerificationFailed(
                                "Invalid PCRs format".to_string(),
                            ))
                        }
                    };

                    for (pcr_key, pcr_value) in pcrs_map {
                        let index = match pcr_key {
                            CborValue::Integer(i) => cbor_integer_to_usize(*i, "PCR index")?,
                            _ => {
                                return Err(Error::AttestationVerificationFailed(
                                    "Invalid PCR index: not an integer".to_string(),
                                ))
                            }
                        };

                        let pcr_bytes = match pcr_value {
                            CborValue::Bytes(b) => b.clone(),
                            _ => {
                                return Err(Error::AttestationVerificationFailed(
                                    "Invalid PCR value".to_string(),
                                ))
                            }
                        };

                        doc.pcrs.insert(index, pcr_bytes);
                    }
                }
                "certificate" => {
                    doc.certificate = match value {
                        CborValue::Bytes(b) => b.clone(),
                        _ => {
                            return Err(Error::AttestationVerificationFailed(
                                "Invalid certificate".to_string(),
                            ))
                        }
                    };
                }
                "cabundle" => {
                    let bundle = match value {
                        CborValue::Array(a) => a,
                        _ => {
                            return Err(Error::AttestationVerificationFailed(
                                "Invalid cabundle".to_string(),
                            ))
                        }
                    };

                    for cert in bundle {
                        let cert_bytes = match cert {
                            CborValue::Bytes(b) => b.clone(),
                            _ => {
                                return Err(Error::AttestationVerificationFailed(
                                    "Invalid certificate in bundle".to_string(),
                                ))
                            }
                        };
                        doc.cabundle.push(cert_bytes);
                    }
                }
                "public_key" => {
                    doc.public_key = match value {
                        CborValue::Bytes(b) => Some(b.clone()),
                        _ => None,
                    };
                }
                "user_data" => {
                    doc.user_data = match value {
                        CborValue::Bytes(b) => Some(b.clone()),
                        _ => None,
                    };
                }
                "nonce" => {
                    doc.nonce = match value {
                        CborValue::Bytes(b) => Some(b.clone()),
                        _ => None,
                    };
                }
                _ => {} // Ignore unknown fields
            }
        }

        Ok(doc)
    }

    fn verify_certificate_chain(&self, doc: &AttestationDocument) -> Result<()> {
        // Step 1: Verify the first cert in cabundle matches AWS Nitro root
        if doc.cabundle.is_empty() {
            return Err(Error::AttestationVerificationFailed(
                "Certificate bundle is empty".to_string(),
            ));
        }

        if doc.cabundle[0] != AWS_NITRO_ROOT_CERT {
            return Err(Error::AttestationVerificationFailed(
                "First certificate does not match AWS Nitro root certificate".to_string(),
            ));
        }

        // Step 2: Parse all certificates and check validity
        let mut certs = Vec::new();
        for (i, cert_der) in doc.cabundle.iter().enumerate() {
            let (_, cert) = X509Certificate::from_der(cert_der).map_err(|e| {
                Error::AttestationVerificationFailed(format!(
                    "Failed to parse certificate {}: {:?}",
                    i, e
                ))
            })?;

            // Validity periods are checked against the device clock in
            // `verify_certificate_validity` once the signature is verified.
            certs.push(cert);
        }

        // Parse the leaf certificate
        let (_, leaf_cert) = X509Certificate::from_der(&doc.certificate).map_err(|e| {
            Error::AttestationVerificationFailed(format!(
                "Failed to parse leaf certificate: {:?}",
                e
            ))
        })?;

        // Step 3: Verify the certificate chain signatures
        // AWS Nitro chain: root -> regional -> zonal -> instance -> leaf
        // Each cert must be signed by the PREVIOUS cert in the hierarchical chain

        // Verify each cert (except root) is signed by the previous cert
        for i in 1..certs.len() {
            let cert = &certs[i];
            let cert_der = &doc.cabundle[i];
            let issuer = &certs[i - 1]; // The issuer should be the previous cert in the chain

            // Verify the issuer/subject relationship
            if cert.issuer() != issuer.subject() {
                return Err(Error::AttestationVerificationFailed(format!(
                    "Certificate {} issuer doesn't match certificate {} subject - chain is broken",
                    i,
                    i - 1
                )));
            }

            // Verify the signature
            if !self.verify_cert_signature(cert_der, issuer)? {
                return Err(Error::AttestationVerificationFailed(format!(
                    "Certificate {} signature verification failed (not signed by certificate {})",
                    i,
                    i - 1
                )));
            }
        }

        // Verify the leaf certificate is signed by the last cert in the chain
        if !certs.is_empty() {
            let last_cert = &certs[certs.len() - 1];

            // Verify the issuer/subject relationship
            if leaf_cert.issuer() != last_cert.subject() {
                return Err(Error::AttestationVerificationFailed(
                    "Leaf certificate issuer doesn't match last certificate in chain".to_string(),
                ));
            }

            // Verify the signature
            if !self.verify_cert_signature(&doc.certificate, last_cert)? {
                return Err(Error::AttestationVerificationFailed(
                    "Leaf certificate signature verification failed".to_string(),
                ));
            }
        }

        Ok(())
    }

    /// Checks every certificate's validity window against the device clock,
    /// with [`ATTESTATION_NOT_BEFORE_LEEWAY_MS`] applied to `notBefore`.
    ///
    /// The enclave leaf certificate is issued without backdating and re-issued
    /// roughly every 2 h 45 m, so a device clock a few seconds slow used to
    /// fail right after each re-issue. `notAfter` stays strict, which bounds a
    /// fast device clock by the leaf's remaining validity. A failure is
    /// explained by comparing the device clock with the signed `timestamp`.
    fn verify_certificate_validity(
        &self,
        doc: &AttestationDocument,
        device_now_ms: u64,
    ) -> Result<()> {
        let device_now_s = i64::try_from(device_now_ms / 1000).map_err(|_| {
            Error::AttestationVerificationFailed("Device clock is out of range".to_string())
        })?;
        let leeway_s = (ATTESTATION_NOT_BEFORE_LEEWAY_MS / 1000) as i64;
        let chain = doc
            .cabundle
            .iter()
            .chain(std::iter::once(&doc.certificate))
            .enumerate();
        for (i, cert_der) in chain {
            let (_, cert) = X509Certificate::from_der(cert_der).map_err(|e| {
                Error::AttestationVerificationFailed(format!(
                    "Failed to parse certificate {}: {:?}",
                    i, e
                ))
            })?;
            let validity = cert.validity();
            let not_before_s = validity.not_before.timestamp();
            let not_after_s = validity.not_after.timestamp();
            let not_yet_valid = device_now_s + leeway_s < not_before_s;
            let expired = device_now_s > not_after_s;
            if not_yet_valid || expired {
                return Err(Error::AttestationVerificationFailed(
                    describe_validity_failure(
                        i,
                        if expired { "expired" } else { "not yet valid" },
                        validity.not_before.timestamp() * 1000,
                        validity.not_after.timestamp() * 1000,
                        device_now_ms,
                        doc.timestamp,
                    ),
                ));
            }
        }
        Ok(())
    }

    fn verify_cert_signature(&self, cert_der: &[u8], issuer: &X509Certificate) -> Result<bool> {
        // Parse the certificate to get its TBS (to-be-signed) portion and signature
        let (_, cert) = X509Certificate::from_der(cert_der).map_err(|e| {
            Error::AttestationVerificationFailed(format!(
                "Failed to parse certificate for signature verification: {:?}",
                e
            ))
        })?;

        // The signature algorithm should match what AWS Nitro uses
        let sig_algo = &cert.signature_algorithm;
        let sig_oid = sig_algo.algorithm.to_id_string();

        // AWS Nitro uses ECDSA with P-384 and SHA-384 (OID: 1.2.840.10045.4.3.3)
        if sig_oid != "1.2.840.10045.4.3.3" {
            // Also support P-256 with SHA-256 (OID: 1.2.840.10045.4.3.2) for compatibility
            if sig_oid != "1.2.840.10045.4.3.2" {
                return Ok(false); // Unsupported algorithm
            }
        }

        // Extract the issuer's public key
        let issuer_pubkey = issuer.public_key();

        // The public key is in SubjectPublicKeyInfo format
        // For EC keys, we need to extract the actual EC point
        let pubkey_bytes = issuer_pubkey.raw;

        // Find the EC point in the public key data
        // EC points start with 0x04 (uncompressed) and are 97 bytes for P-384, 65 for P-256
        let ec_point = if sig_oid == "1.2.840.10045.4.3.3" {
            // P-384: 97 bytes (0x04 + 48 bytes X + 48 bytes Y)
            extract_ec_point(pubkey_bytes, 97)
        } else {
            // P-256: 65 bytes (0x04 + 32 bytes X + 32 bytes Y)
            extract_ec_point(pubkey_bytes, 65)
        }?;

        // Get the TBS certificate data and signature
        let tbs_cert = cert.tbs_certificate.as_ref();
        let signature = cert.signature_value.as_ref();

        // Verify the signature using ring
        let verification_alg = if sig_oid == "1.2.840.10045.4.3.3" {
            &signature::ECDSA_P384_SHA384_ASN1
        } else {
            &signature::ECDSA_P256_SHA256_ASN1
        };

        let public_key = signature::UnparsedPublicKey::new(verification_alg, ec_point);

        public_key
            .verify(tbs_cert, signature)
            .map(|_| true)
            .map_err(|_| {
                Error::AttestationVerificationFailed(
                    "Certificate signature verification failed".to_string(),
                )
            })
    }

    fn verify_signature(
        &self,
        protected: &[u8],
        payload: &[u8],
        signature_bytes: &[u8],
        doc: &AttestationDocument,
    ) -> Result<()> {
        // Parse the leaf certificate
        let (_, cert) = X509Certificate::from_der(&doc.certificate).map_err(|e| {
            Error::AttestationVerificationFailed(format!(
                "Failed to parse leaf certificate: {:?}",
                e
            ))
        })?;

        // Extract the public key bytes from the certificate
        let public_key_info = cert.public_key();

        // AWS Nitro uses P-384, extract the EC point properly
        // P-384: 97 bytes (0x04 + 48 bytes X + 48 bytes Y)
        let public_key_bytes = extract_ec_point(public_key_info.raw, 97)?;

        // Create the COSE_Sign1 signature structure
        // This follows the COSE specification for the data to be signed
        let sig_structure = create_sig_structure(protected, payload)?;

        // For ECDSA P-384 with SHA-384 (which is what AWS Nitro uses)
        // AWS Nitro uses raw signatures (r||s), not ASN.1 encoded
        let public_key = signature::UnparsedPublicKey::new(
            &signature::ECDSA_P384_SHA384_FIXED,
            public_key_bytes,
        );

        // Verify the signature
        public_key
            .verify(&sig_structure, signature_bytes)
            .map_err(|_| {
                Error::AttestationVerificationFailed("Signature verification failed".to_string())
            })?;

        Ok(())
    }

    fn verify_pcrs(
        &self,
        doc: &AttestationDocument,
        expected: &std::collections::HashMap<usize, Vec<u8>>,
    ) -> Result<()> {
        for (index, expected_value) in expected {
            match doc.pcrs.get(index) {
                Some(actual_value) => {
                    if actual_value != expected_value {
                        return Err(Error::AttestationVerificationFailed(format!(
                            "PCR{} mismatch",
                            index
                        )));
                    }
                }
                None => {
                    return Err(Error::AttestationVerificationFailed(format!(
                        "PCR{} missing",
                        index
                    )));
                }
            }
        }
        Ok(())
    }
}

fn extract_ec_point(pubkey_bytes: &[u8], expected_size: usize) -> Result<&[u8]> {
    // The public key is in SubjectPublicKeyInfo format (ASN.1 DER encoded)
    // We need to extract the actual EC point from the BIT STRING

    // The structure is:
    // SEQUENCE {
    //   algorithm AlgorithmIdentifier,
    //   subjectPublicKey BIT STRING
    // }

    // For EC keys, x509-parser gives us the raw bytes which includes the full
    // SubjectPublicKeyInfo structure. The EC point is at the end after the
    // algorithm identifier and is preceded by a BIT STRING tag.

    // Look for BIT STRING tag (0x03) followed by length and unused bits (0x00)
    // The EC point follows immediately after
    for i in 0..pubkey_bytes.len() {
        if pubkey_bytes[i] == 0x03 {
            // BIT STRING tag
            if i + 2 < pubkey_bytes.len() {
                // Next byte is length (for EC keys, usually 0x42 for P-256 or 0x62 for P-384)
                // Then 0x00 for no unused bits
                // Then 0x04 for uncompressed point
                if i + 3 < pubkey_bytes.len()
                    && pubkey_bytes[i + 2] == 0x00
                    && pubkey_bytes[i + 3] == 0x04
                {
                    let ec_point_start = i + 3;
                    let remaining = &pubkey_bytes[ec_point_start..];
                    if remaining.len() == expected_size {
                        return Ok(remaining);
                    }
                }
            }
        }
    }

    // Fallback: The x509-parser might have already extracted just the key material
    // In this case, look for the uncompressed point marker (0x04) at the expected position
    if pubkey_bytes.len() >= expected_size {
        // Try from the end (most common case with x509-parser)
        let from_end = &pubkey_bytes[pubkey_bytes.len() - expected_size..];
        if from_end[0] == 0x04 {
            return Ok(from_end);
        }

        // Try from a typical offset (after algorithm OID and parameters)
        // For EC keys, this is often around offset 23-27
        for offset in [23, 24, 25, 26, 27].iter() {
            if *offset + expected_size <= pubkey_bytes.len() {
                let candidate = &pubkey_bytes[*offset..*offset + expected_size];
                if candidate[0] == 0x04 {
                    return Ok(candidate);
                }
            }
        }
    }

    Err(Error::AttestationVerificationFailed(format!(
        "Failed to extract EC public key point (expected {} bytes, pubkey is {} bytes)",
        expected_size,
        pubkey_bytes.len()
    )))
}

fn cbor_integer_to_u64(value: ciborium::value::Integer, field_name: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| {
        Error::AttestationVerificationFailed(format!(
            "Invalid {}: negative or out of range value",
            field_name
        ))
    })
}

fn cbor_integer_to_usize(value: ciborium::value::Integer, field_name: &str) -> Result<usize> {
    usize::try_from(value).map_err(|_| {
        Error::AttestationVerificationFailed(format!(
            "Invalid {}: negative or out of range value",
            field_name
        ))
    })
}

fn create_sig_structure(protected: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
    // Create the COSE_Sign1 signature structure as a CBOR array
    // ["Signature1", protected, external_aad, payload]
    let sig_structure = CborValue::Array(vec![
        CborValue::Text("Signature1".to_string()),
        CborValue::Bytes(protected.to_vec()),
        CborValue::Bytes(vec![]), // empty external AAD
        CborValue::Bytes(payload.to_vec()),
    ]);

    // Encode to CBOR bytes
    cbor::to_vec(&sig_structure)
}

/// Leeway applied to certificate `notBefore` so a device clock that runs
/// slightly slow does not reject a freshly issued enclave leaf certificate.
/// `notAfter` gets no leeway. The TypeScript SDK applies the same values.
pub const ATTESTATION_NOT_BEFORE_LEEWAY_MS: u64 = 5 * 60 * 1000;

/// Device/enclave difference above which a validity failure is reported as a
/// device clock problem rather than a transient certificate problem.
pub const NOTICEABLE_CLOCK_SKEW_MS: u64 = 60 * 1000;

/// Prefix of every attestation failure message that blames the device clock.
/// [`crate::Error::is_device_clock_problem`] recognizes it so clients can show
/// their own clock guidance without surfacing SDK internals.
const DEVICE_CLOCK_MESSAGE_PREFIX: &str = "This device's clock is about";

impl crate::Error {
    /// True when this is an attestation failure caused by the device clock
    /// disagreeing with the enclave's signed timestamp (wrong date, time or
    /// time zone on the device), as opposed to an untrusted or invalid
    /// document.
    pub fn is_device_clock_problem(&self) -> bool {
        matches!(
            self,
            crate::Error::AttestationVerificationFailed(message)
                if message.starts_with(DEVICE_CLOCK_MESSAGE_PREFIX)
        )
    }
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn format_time_ms(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| format!("{ms} ms since epoch"))
}

fn describe_duration_ms(ms: u64) -> String {
    const MINUTE: u64 = 60 * 1000;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    for (name, size) in [("day", DAY), ("hour", HOUR), ("minute", MINUTE)] {
        if ms >= size {
            let count = (ms as f64 / size as f64).round() as u64;
            let plural = if count == 1 { "" } else { "s" };
            return format!("{count} {name}{plural}");
        }
    }
    format!("{} seconds", (ms as f64 / 1000.0).round() as u64)
}

/// Builds the user-facing message for a certificate outside its validity
/// window, blaming the device clock when it disagrees with the enclave's
/// signed timestamp by a noticeable amount.
fn describe_validity_failure(
    index: usize,
    state: &str,
    not_before_ms: i64,
    not_after_ms: i64,
    device_now_ms: u64,
    enclave_now_ms: u64,
) -> String {
    let (skew, direction) = if device_now_ms >= enclave_now_ms {
        (device_now_ms - enclave_now_ms, "ahead of")
    } else {
        (enclave_now_ms - device_now_ms, "behind")
    };
    let device = format_time_ms(i64::try_from(device_now_ms).unwrap_or(i64::MAX));
    if skew >= NOTICEABLE_CLOCK_SKEW_MS {
        format!(
            "{DEVICE_CLOCK_MESSAGE_PREFIX} {} {} the secure enclave (device: {}, enclave: {}), so the enclave's certificate looks {}. Check the device's date, time and time zone settings, then try again.",
            describe_duration_ms(skew),
            direction,
            device,
            format_time_ms(i64::try_from(enclave_now_ms).unwrap_or(i64::MAX)),
            state
        )
    } else {
        format!(
            "The secure enclave's certificate {} is {} on this device's clock (valid {} to {}, device: {}). Try again in a moment; if it keeps happening, check the device's date, time and time zone settings.",
            index,
            state,
            format_time_ms(not_before_ms),
            format_time_ms(not_after_ms),
            device
        )
    }
}

#[cfg(feature = "mock-attestation")]
pub fn create_mock_attestation_document(nonce: &str) -> Result<String> {
    use std::collections::HashMap;

    let mut pcrs = HashMap::new();
    pcrs.insert(0, vec![0u8; 48]); // Mock PCR0

    let doc = AttestationDocument {
        module_id: "mock-module".to_string(),
        timestamp: chrono::Utc::now().timestamp() as u64,
        digest: "SHA384".to_string(),
        pcrs,
        certificate: vec![0u8; 256],    // Mock certificate
        cabundle: vec![vec![0u8; 256]], // Mock CA bundle
        public_key: None,
        user_data: None,
        nonce: Some(nonce.as_bytes().to_vec()),
    };

    // Create a mock COSE_Sign1 structure
    let payload = cbor::to_vec(&doc)?;
    let protected = vec![0u8; 32]; // Mock protected header
    let signature = vec![0u8; 64]; // Mock signature

    let cose_sign1 = vec![
        CborValue::Bytes(protected),
        CborValue::Map(Vec::new()), // Empty unprotected headers
        CborValue::Bytes(payload),
        CborValue::Bytes(signature),
    ];

    let cose_bytes = cbor::to_vec(&CborValue::Array(cose_sign1))?;
    Ok(BASE64.encode(cose_bytes))
}

#[cfg(all(test, feature = "mock-attestation"))]
mod tests {
    use super::*;

    #[test]
    fn default_verifier_never_accepts_feature_gated_mock_documents() {
        let nonce = "test-nonce";
        let document = create_mock_attestation_document(nonce).unwrap();

        let error = AttestationVerifier::new()
            .verify_attestation_document(&document, nonce)
            .unwrap_err();

        assert!(matches!(error, Error::AttestationVerificationFailed(_)));
    }
}

#[cfg(test)]
mod clock_policy_tests {
    use super::*;

    /// A real production document from 2024-10-28. Its leaf certificate became
    /// valid 361 s before the document was signed and expired three hours later.
    const FIXTURE: &str = include_str!("test_fixtures/nitro_attestation_document_2024-10-28.b64");
    const NONCE: &str = "cc6b95ef-a0d7-477d-90f2-36cc088d2449";
    const SIGNED_AT_MS: u64 = 1_730_141_656_206;
    const LEAF_NOT_BEFORE_MS: u64 = SIGNED_AT_MS - 361 * 1000 - 206;
    const LEAF_NOT_AFTER_MS: u64 = LEAF_NOT_BEFORE_MS + 3 * 60 * 60 * 1000 + 3 * 1000;
    const SECOND_MS: u64 = 1000;
    const DAY_MS: u64 = 24 * 60 * 60 * SECOND_MS;

    fn verify_at(device_now_ms: u64) -> Result<AttestationDocument> {
        AttestationVerifier::new().verify_attestation_document_at(
            FIXTURE.trim(),
            |nonce| {
                assert_eq!(nonce, NONCE.as_bytes());
                Ok(())
            },
            device_now_ms,
        )
    }

    fn failure_at(device_now_ms: u64) -> String {
        verify_at(device_now_ms).unwrap_err().to_string()
    }

    #[test]
    fn fixture_leaf_validity_matches_the_certificate() {
        let doc = verify_at(SIGNED_AT_MS).unwrap();
        let (_, leaf) = X509Certificate::from_der(&doc.certificate).unwrap();
        assert_eq!(
            leaf.validity().not_before.timestamp() as u64 * 1000,
            LEAF_NOT_BEFORE_MS
        );
        assert_eq!(
            leaf.validity().not_after.timestamp() as u64 * 1000,
            LEAF_NOT_AFTER_MS
        );
    }

    #[test]
    fn real_document_verifies_when_the_device_clock_matches_the_enclave() {
        let doc = verify_at(SIGNED_AT_MS).unwrap();
        assert_eq!(doc.module_id, "i-06c79bf817127030a-enc0192d3d4945e0432");
        assert_eq!(doc.timestamp, SIGNED_AT_MS);
    }

    #[test]
    fn device_clock_slightly_behind_a_fresh_leaf_verifies_within_the_leeway() {
        for device_now_ms in [
            LEAF_NOT_BEFORE_MS - SECOND_MS,
            LEAF_NOT_BEFORE_MS - 60 * SECOND_MS,
            LEAF_NOT_BEFORE_MS - ATTESTATION_NOT_BEFORE_LEEWAY_MS,
        ] {
            verify_at(device_now_ms).unwrap();
        }
    }

    #[test]
    fn device_clock_behind_by_more_than_the_leeway_is_a_clock_error() {
        let message = failure_at(LEAF_NOT_BEFORE_MS - ATTESTATION_NOT_BEFORE_LEEWAY_MS - SECOND_MS);
        assert!(
            message.contains("about 11 minutes behind the secure enclave"),
            "{message}"
        );
        assert!(message.contains("looks not yet valid"), "{message}");
        assert!(
            message.contains("date, time and time zone settings"),
            "{message}"
        );
    }

    #[test]
    fn fast_device_clock_is_accepted_only_while_the_leaf_is_valid() {
        verify_at(LEAF_NOT_AFTER_MS - SECOND_MS).unwrap();
        let message = failure_at(LEAF_NOT_AFTER_MS + SECOND_MS);
        assert!(
            message.contains("about 3 hours ahead of the secure enclave"),
            "{message}"
        );
        assert!(message.contains("looks expired"), "{message}");
    }

    #[test]
    fn device_clock_days_off_names_the_difference() {
        let ahead = failure_at(SIGNED_AT_MS + 3 * DAY_MS);
        assert!(
            ahead.contains("about 3 days ahead of the secure enclave"),
            "{ahead}"
        );
        let behind = failure_at(SIGNED_AT_MS - 3 * DAY_MS);
        assert!(
            behind.contains("about 3 days behind the secure enclave"),
            "{behind}"
        );
    }

    #[test]
    fn clock_problems_are_distinguishable_from_other_attestation_failures() {
        let clock_error = verify_at(SIGNED_AT_MS + 3 * DAY_MS).unwrap_err();
        assert!(clock_error.is_device_clock_problem());

        let other = Error::AttestationVerificationFailed("PCR0 mismatch".to_string());
        assert!(!other.is_device_clock_problem());
        assert!(!Error::Session("expired".to_string()).is_device_clock_problem());
    }

    #[test]
    fn todays_clock_rejects_the_2024_document() {
        let message = failure_at(current_time_ms());
        assert!(message.contains("ahead of the secure enclave"), "{message}");
    }

    #[test]
    fn tampered_timestamp_fails_signature_verification_first() {
        let raw = BASE64.decode(FIXTURE.trim()).unwrap();
        let mut cose = match cbor::from_slice::<CborValue>(&raw).unwrap() {
            CborValue::Array(items) => items,
            other => panic!("unexpected COSE structure: {other:?}"),
        };
        let payload = match &cose[2] {
            CborValue::Bytes(bytes) => bytes.clone(),
            other => panic!("unexpected payload: {other:?}"),
        };
        let mut fields = match cbor::from_slice::<CborValue>(&payload).unwrap() {
            CborValue::Map(fields) => fields,
            other => panic!("unexpected payload map: {other:?}"),
        };
        for (key, value) in fields.iter_mut() {
            if matches!(key, CborValue::Text(name) if name == "timestamp") {
                *value = CborValue::Integer(current_time_ms().into());
            }
        }
        cose[2] = CborValue::Bytes(cbor::to_vec(&CborValue::Map(fields)).unwrap());
        let forged = BASE64.encode(cbor::to_vec(&CborValue::Array(cose)).unwrap());

        let error = AttestationVerifier::new()
            .verify_attestation_document_at(&forged, |_| Ok(()), SIGNED_AT_MS)
            .unwrap_err();
        assert!(
            error.to_string().contains("Signature verification failed"),
            "{error}"
        );
    }
}
