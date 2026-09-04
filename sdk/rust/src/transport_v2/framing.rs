use bytes::{Bytes, BytesMut};
use serde::Deserialize;
use zeroize::Zeroizing;

use super::{
    crypto::{ResponseOpener, RECORD_TAG_BYTES},
    envelope::LogicalHeader,
    Result, TransportV2Error,
};

const CIPHERTEXT_LENGTH_BYTES: usize = 4;
const MAX_RESPONSE_CHUNK_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_METADATA_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_RECORD_PLAINTEXT_BYTES: usize = 1 + MAX_RESPONSE_CHUNK_BYTES;
const MAX_RESPONSE_RECORD_CIPHERTEXT_BYTES: usize =
    MAX_RESPONSE_RECORD_PLAINTEXT_BYTES + RECORD_TAG_BYTES;
const MAX_RESPONSE_HEADER_COUNT: usize = 32;
const MAX_RESPONSE_ERROR_CODE_BYTES: usize = 64;

const START_TAG: u8 = 1;
const CHUNK_TAG: u8 = 2;
const END_TAG: u8 = 3;
const ERROR_TAG: u8 = 4;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ResponseEvent {
    Start {
        status: u16,
        headers: Vec<LogicalHeader>,
    },
    Chunk(Bytes),
    End,
    Error {
        code: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponseStart {
    status: u16,
    headers: Vec<LogicalHeader>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponseFailure {
    code: String,
}

enum DecodeState {
    AwaitingStart,
    Open,
    PendingTerminal(ResponseEvent),
    Finished,
}

/// Incrementally authenticates one response without buffering the stream.
///
/// The terminal record is withheld until carrier EOF, making bytes after a
/// valid End/Error detectable rather than silently ignored.
pub(super) struct ResponseDecoder {
    opener: ResponseOpener,
    state: DecodeState,
    length_prefix: [u8; CIPHERTEXT_LENGTH_BYTES],
    length_prefix_bytes: usize,
    expected_ciphertext_bytes: Option<usize>,
    ciphertext: BytesMut,
}

impl ResponseDecoder {
    pub(super) fn new(opener: ResponseOpener) -> Self {
        Self {
            opener,
            state: DecodeState::AwaitingStart,
            length_prefix: [0; CIPHERTEXT_LENGTH_BYTES],
            length_prefix_bytes: 0,
            expected_ciphertext_bytes: None,
            ciphertext: BytesMut::new(),
        }
    }

    pub(super) fn push(&mut self, mut input: &[u8]) -> Result<Vec<ResponseEvent>> {
        if matches!(self.state, DecodeState::Finished) {
            return Err(TransportV2Error::PostTerminalData);
        }
        if matches!(self.state, DecodeState::PendingTerminal(_)) && !input.is_empty() {
            return Err(TransportV2Error::PostTerminalData);
        }

        let mut events = Vec::new();
        while !input.is_empty() {
            if self.expected_ciphertext_bytes.is_none() {
                let needed = CIPHERTEXT_LENGTH_BYTES - self.length_prefix_bytes;
                let take = needed.min(input.len());
                self.length_prefix[self.length_prefix_bytes..self.length_prefix_bytes + take]
                    .copy_from_slice(&input[..take]);
                self.length_prefix_bytes += take;
                input = &input[take..];
                if self.length_prefix_bytes < CIPHERTEXT_LENGTH_BYTES {
                    continue;
                }

                let length = u32::from_be_bytes(self.length_prefix) as usize;
                if !(RECORD_TAG_BYTES..=MAX_RESPONSE_RECORD_CIPHERTEXT_BYTES).contains(&length) {
                    return Err(TransportV2Error::InvalidFrame);
                }
                self.expected_ciphertext_bytes = Some(length);
                self.ciphertext.reserve(length);
            }

            let expected = self
                .expected_ciphertext_bytes
                .expect("response frame length was just established");
            let needed = expected - self.ciphertext.len();
            let take = needed.min(input.len());
            self.ciphertext.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.ciphertext.len() < expected {
                continue;
            }

            let ciphertext = self.ciphertext.split().freeze();
            self.expected_ciphertext_bytes = None;
            self.length_prefix_bytes = 0;
            let event = self.decode_record(&ciphertext)?;
            match event {
                ResponseEvent::End | ResponseEvent::Error { .. } => {
                    self.state = DecodeState::PendingTerminal(event);
                    if !input.is_empty() {
                        return Err(TransportV2Error::PostTerminalData);
                    }
                }
                event => events.push(event),
            }
        }
        Ok(events)
    }

    pub(super) fn finish(&mut self) -> Result<ResponseEvent> {
        if self.length_prefix_bytes != 0
            || self.expected_ciphertext_bytes.is_some()
            || !self.ciphertext.is_empty()
        {
            return Err(TransportV2Error::TruncatedResponse);
        }
        match std::mem::replace(&mut self.state, DecodeState::Finished) {
            DecodeState::PendingTerminal(event) => Ok(event),
            DecodeState::AwaitingStart | DecodeState::Open => {
                Err(TransportV2Error::TruncatedResponse)
            }
            DecodeState::Finished => Err(TransportV2Error::PostTerminalData),
        }
    }

    fn decode_record(&mut self, ciphertext: &[u8]) -> Result<ResponseEvent> {
        let plaintext = Zeroizing::new(self.opener.open_next(ciphertext)?);
        let event = decode_plaintext_record(&plaintext)?;

        match (&self.state, &event) {
            (DecodeState::AwaitingStart, ResponseEvent::Start { .. }) => {
                self.state = DecodeState::Open;
            }
            (DecodeState::Open, ResponseEvent::Chunk(_))
            | (DecodeState::Open, ResponseEvent::End)
            | (DecodeState::Open, ResponseEvent::Error { .. }) => {}
            _ => return Err(TransportV2Error::InvalidSequence),
        }
        Ok(event)
    }
}

fn decode_plaintext_record(encoded: &[u8]) -> Result<ResponseEvent> {
    let (&tag, payload) = encoded
        .split_first()
        .ok_or(TransportV2Error::InvalidRecord)?;
    match tag {
        START_TAG => {
            if payload.len() > MAX_RESPONSE_METADATA_BYTES {
                return Err(TransportV2Error::InvalidRecord);
            }
            let start: ResponseStart =
                serde_json::from_slice(payload).map_err(|_| TransportV2Error::InvalidRecord)?;
            if !(200..=599).contains(&start.status)
                || start.headers.len() > MAX_RESPONSE_HEADER_COUNT
            {
                return Err(TransportV2Error::InvalidRecord);
            }
            for header in &start.headers {
                LogicalHeader::new(header.name().to_owned(), header.value().to_owned())?;
            }
            Ok(ResponseEvent::Start {
                status: start.status,
                headers: start.headers,
            })
        }
        CHUNK_TAG if payload.len() <= MAX_RESPONSE_CHUNK_BYTES => {
            Ok(ResponseEvent::Chunk(Bytes::copy_from_slice(payload)))
        }
        END_TAG if payload.is_empty() => Ok(ResponseEvent::End),
        ERROR_TAG => {
            if payload.len() > MAX_RESPONSE_METADATA_BYTES {
                return Err(TransportV2Error::InvalidRecord);
            }
            let failure: ResponseFailure =
                serde_json::from_slice(payload).map_err(|_| TransportV2Error::InvalidRecord)?;
            if failure.code.is_empty()
                || failure.code.len() > MAX_RESPONSE_ERROR_CODE_BYTES
                || !failure
                    .code
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            {
                return Err(TransportV2Error::InvalidRecord);
            }
            Ok(ResponseEvent::Error { code: failure.code })
        }
        _ => Err(TransportV2Error::InvalidRecord),
    }
}

#[cfg(test)]
pub(super) fn frame_response_for_test(
    secrets: &super::crypto::SessionSecrets,
    request_id: super::envelope::RequestId,
    sequence: u64,
    plaintext: &[u8],
) -> Vec<u8> {
    let ciphertext =
        super::crypto::seal_response_for_test(secrets, request_id, sequence, plaintext);
    let mut frame = Vec::with_capacity(4 + ciphertext.len());
    frame.extend_from_slice(&(ciphertext.len() as u32).to_be_bytes());
    frame.extend_from_slice(&ciphertext);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport_v2::crypto::{
        session_from_shared_for_test, HandshakeTranscript, SessionId, SessionSecrets,
    };
    use crate::transport_v2::envelope::RequestId;
    use std::sync::Arc;

    fn secrets(marker: u8) -> Arc<SessionSecrets> {
        Arc::new(session_from_shared_for_test(
            [marker; 32],
            HandshakeTranscript::new([0x11; 32], [0x22; 32], [0x33; 32]),
        ))
    }

    fn response_frames(secrets: &SessionSecrets, request_id: RequestId) -> Vec<u8> {
        [
            [&[START_TAG][..], &br#"{"status":200,"headers":[]}"#[..]].concat(),
            [&[CHUNK_TAG][..], &b"hello"[..]].concat(),
            vec![END_TAG],
        ]
        .into_iter()
        .enumerate()
        .flat_map(|(sequence, plaintext)| {
            frame_response_for_test(secrets, request_id, sequence as u64, &plaintext)
        })
        .collect()
    }

    #[test]
    fn fragmented_stream_is_incremental_and_terminal_waits_for_eof() {
        let secrets = secrets(0x44);
        let request_id = RequestId::from_bytes([7; 16]);
        let wire = response_frames(&secrets, request_id);
        let mut decoder = ResponseDecoder::new(secrets.response_opener(request_id).unwrap());
        let mut events = Vec::new();
        for byte in wire {
            events.extend(decoder.push(&[byte]).unwrap());
        }
        assert!(matches!(
            events[0],
            ResponseEvent::Start { status: 200, .. }
        ));
        assert_eq!(
            events[1],
            ResponseEvent::Chunk(Bytes::from_static(b"hello"))
        );
        assert_eq!(events.len(), 2);
        assert_eq!(decoder.finish().unwrap(), ResponseEvent::End);
    }

    #[test]
    fn eof_without_terminal_and_bytes_after_terminal_fail_closed() {
        let secrets = secrets(0x44);
        let request_id = RequestId::from_bytes([8; 16]);
        let start_plaintext = [&[START_TAG][..], &br#"{"status":200,"headers":[]}"#[..]].concat();
        let start = frame_response_for_test(&secrets, request_id, 0, &start_plaintext);
        let mut decoder = ResponseDecoder::new(secrets.response_opener(request_id).unwrap());
        decoder.push(&start).unwrap();
        assert!(matches!(
            decoder.finish(),
            Err(TransportV2Error::TruncatedResponse)
        ));

        let wire = response_frames(&secrets, request_id);
        let mut decoder = ResponseDecoder::new(secrets.response_opener(request_id).unwrap());
        assert!(matches!(
            decoder.push(&[wire, vec![0]].concat()),
            Err(TransportV2Error::PostTerminalData)
        ));
    }

    #[test]
    fn response_transplant_and_reordering_fail_authentication() {
        let first = secrets(0x44);
        let second = secrets(0x45);
        assert_ne!(first.session_id(), SessionId::from_bytes([0; 16]));
        let request_id = RequestId::from_bytes([9; 16]);
        let start_plaintext = [&[START_TAG][..], &br#"{"status":200,"headers":[]}"#[..]].concat();
        let start = frame_response_for_test(&first, request_id, 0, &start_plaintext);

        let mut wrong_session = ResponseDecoder::new(second.response_opener(request_id).unwrap());
        assert!(matches!(
            wrong_session.push(&start),
            Err(TransportV2Error::Authentication)
        ));
        let mut wrong_request = ResponseDecoder::new(
            first
                .response_opener(RequestId::from_bytes([10; 16]))
                .unwrap(),
        );
        assert!(matches!(
            wrong_request.push(&start),
            Err(TransportV2Error::Authentication)
        ));
    }

    #[test]
    fn repeated_response_headers_are_valid_and_ordered() {
        let mut encoded = vec![START_TAG];
        encoded.extend_from_slice(
            br#"{"status":200,"headers":[{"name":"warning","value":"first"},{"name":"warning","value":"second"}]}"#,
        );

        let ResponseEvent::Start { headers, .. } = decode_plaintext_record(&encoded).unwrap()
        else {
            panic!("expected start record");
        };
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].value(), "first");
        assert_eq!(headers[1].value(), "second");
    }
}
