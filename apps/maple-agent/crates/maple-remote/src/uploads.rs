//! Uploads: image bytes a client sends ahead of `run.send`, on streams it
//! opens.
//!
//! An image inline in a `run.send` request would be capped by the control
//! frame limit, so the client streams it first under an id it minted and
//! the request names the id. One connection keeps its own uploads: at most
//! [`MAX_UPLOADS_IN_FLIGHT`] collecting and [`MAX_COMPLETED_UPLOADS`]
//! finished but not yet named by a request, oldest dropped first. A
//! `run.send` consumes the uploads it names; the connection's teardown
//! drops the rest.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use base64::Engine as _;

use crate::frame::{Frame, FrameKind};
use crate::streams::{MAX_IMAGE_BYTES, StreamOpen, StreamReceivers, UPLOAD_PURPOSE, close_frame};

/// Bytes one upload may carry: the host's own limit per image.
pub const MAX_UPLOAD_BYTES: usize = MAX_IMAGE_BYTES;
/// Uploads one connection may have collecting at once.
pub const MAX_UPLOADS_IN_FLIGHT: usize = 4;
/// Finished uploads one connection keeps until a `run.send` names them.
pub const MAX_COMPLETED_UPLOADS: usize = 16;

/// Longest upload id or media type accepted, so neither can pad a log or
/// an error.
const MAX_LABEL_CHARS: usize = 128;

/// One finished upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upload {
    pub mime: String,
    pub bytes: Vec<u8>,
}

impl Upload {
    /// The `data:` URL the runtime takes, rebuilt the way the client
    /// built it before it streamed the bytes.
    pub fn data_url(&self) -> String {
        let mut url =
            String::with_capacity(self.mime.len() + 16 + self.bytes.len().div_ceil(3) * 4);
        url.push_str("data:");
        url.push_str(&self.mime);
        url.push_str(";base64,");
        base64::engine::general_purpose::STANDARD.encode_string(&self.bytes, &mut url);
        url
    }
}

/// The uploads of one connection.
pub struct Uploads {
    receivers: StreamReceivers<String>,
    /// Media types of the uploads still collecting, by id.
    in_flight: Mutex<HashMap<String, String>>,
    /// Finished uploads, oldest first.
    completed: Mutex<VecDeque<(String, Upload)>>,
}

impl Default for Uploads {
    fn default() -> Self {
        Self {
            receivers: StreamReceivers::new(MAX_UPLOAD_BYTES),
            in_flight: Mutex::new(HashMap::new()),
            completed: Mutex::new(VecDeque::new()),
        }
    }
}

impl Uploads {
    /// A frame on a client-opened channel. Returns the frame to send
    /// back: credit while the bytes flow, a `Close` acknowledging a
    /// finished upload, or a `Close` naming why one was refused.
    pub fn on_frame(&self, frame: &Frame) -> Option<Frame> {
        match frame.kind {
            FrameKind::Open => self
                .on_open(frame.channel, &frame.payload)
                .err()
                .map(|reason| close_frame(frame.channel, Some(&reason))),
            FrameKind::Data => match self.receivers.on_data(frame.channel, &frame.payload) {
                Ok(credit) => credit,
                Err((upload_id, reason)) => {
                    self.forget_in_flight(&upload_id);
                    log::debug!("upload {upload_id} refused: {reason}");
                    Some(close_frame(frame.channel, Some(&reason)))
                }
            },
            FrameKind::Close => {
                let (upload_id, result) = self.receivers.on_close(frame.channel, &frame.payload)?;
                let mime = self.forget_in_flight(&upload_id)?;
                match result {
                    Ok(bytes) => {
                        self.keep(upload_id, Upload { mime, bytes });
                        Some(close_frame(frame.channel, None))
                    }
                    Err(reason) => {
                        log::debug!("upload {upload_id} ended early: {reason}");
                        Some(close_frame(frame.channel, Some(&reason)))
                    }
                }
            }
            // A client does not grant credit on its own stream.
            FrameKind::Credit => None,
        }
    }

    fn on_open(&self, channel: u16, payload: &[u8]) -> Result<(), String> {
        let open: StreamOpen =
            serde_json::from_slice(payload).map_err(|error| format!("bad stream open: {error}"))?;
        if open.purpose != UPLOAD_PURPOSE {
            return Err(format!("a client cannot open a {} stream", open.purpose));
        }
        let upload_id = open
            .upload_id
            .filter(|id| is_clean_label(id))
            .ok_or_else(|| "an upload needs an id".to_string())?;
        let mime = open
            .mime
            .filter(|mime| is_clean_label(mime) && !mime.contains([',', ';']))
            .ok_or_else(|| "an upload needs a media type".to_string())?;
        let len = open
            .len
            .ok_or_else(|| "an upload needs its length".to_string())?;
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if in_flight.len() >= MAX_UPLOADS_IN_FLIGHT {
            return Err(format!(
                "at most {MAX_UPLOADS_IN_FLIGHT} uploads may be in flight"
            ));
        }
        if in_flight.contains_key(&upload_id) || self.has_completed(&upload_id) {
            return Err(format!("upload {upload_id} already exists"));
        }
        self.receivers
            .accept(channel, upload_id.clone(), Some(len))
            .map_err(|reason| format!("upload {upload_id} refused: {reason}"))?;
        in_flight.insert(upload_id, mime);
        Ok(())
    }

    fn forget_in_flight(&self, upload_id: &str) -> Option<String> {
        self.in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(upload_id)
    }

    fn has_completed(&self, upload_id: &str) -> bool {
        self.completed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|(id, _)| id == upload_id)
    }

    /// Keep a finished upload, dropping the oldest past the limit.
    fn keep(&self, upload_id: String, upload: Upload) {
        let mut completed = self
            .completed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        completed.push_back((upload_id, upload));
        while completed.len() > MAX_COMPLETED_UPLOADS {
            if let Some((dropped, _)) = completed.pop_front() {
                log::debug!("upload {dropped} dropped: too many unreferenced uploads");
            }
        }
    }

    /// Take every upload named, in order, or none of them: a request that
    /// names one unknown id must not consume the others.
    pub fn take_all(&self, upload_ids: &[String]) -> Result<Vec<Upload>, String> {
        let mut completed = self
            .completed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut positions = Vec::with_capacity(upload_ids.len());
        for upload_id in upload_ids {
            let position = completed
                .iter()
                .position(|(id, _)| id == upload_id)
                .filter(|position| !positions.contains(position))
                .ok_or_else(|| format!("unknown or incomplete upload {upload_id}"))?;
            positions.push(position);
        }
        // Remove from the back so earlier positions stay valid, then
        // restore the order the request named them in.
        let mut order: Vec<(usize, usize)> = positions.into_iter().enumerate().collect();
        order.sort_by_key(|(_, position)| std::cmp::Reverse(*position));
        let mut taken: Vec<(usize, Upload)> = order
            .into_iter()
            .filter_map(|(index, position)| {
                completed
                    .remove(position)
                    .map(|(_, upload)| (index, upload))
            })
            .collect();
        taken.sort_by_key(|(index, _)| *index);
        Ok(taken.into_iter().map(|(_, upload)| upload).collect())
    }

    /// Uploads still collecting.
    pub fn in_flight_count(&self) -> usize {
        self.receivers.open_count()
    }

    /// Uploads finished and not yet named.
    pub fn completed_count(&self) -> usize {
        self.completed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

/// Short, printable, and without whitespace: fit for a log line and for
/// the header of a `data:` URL.
fn is_clean_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL_CHARS
        && label.bytes().all(|byte| byte.is_ascii_graphic())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::MAX_STREAM_FRAME_BYTES;
    use crate::streams::{CREDIT_REFILL, StreamClose};
    use bytes::Bytes;

    fn open_frame(channel: u16, upload_id: &str, len: u64) -> Frame {
        let open = StreamOpen {
            purpose: UPLOAD_PURPOSE.into(),
            request_id: None,
            upload_id: Some(upload_id.into()),
            mime: Some("image/png".into()),
            len: Some(len),
        };
        Frame {
            channel,
            kind: FrameKind::Open,
            payload: Bytes::from(serde_json::to_vec(&open).unwrap()),
        }
    }

    fn data_frame(channel: u16, bytes: &[u8]) -> Frame {
        Frame {
            channel,
            kind: FrameKind::Data,
            payload: Bytes::copy_from_slice(bytes),
        }
    }

    fn refusal(frame: Option<Frame>) -> String {
        let frame = frame.expect("a refusal");
        assert_eq!(frame.kind, FrameKind::Close);
        serde_json::from_slice::<StreamClose>(&frame.payload)
            .unwrap()
            .error
            .expect("an error")
    }

    fn upload(uploads: &Uploads, channel: u16, id: &str, bytes: &[u8]) {
        assert!(
            uploads
                .on_frame(&open_frame(channel, id, bytes.len() as u64))
                .is_none()
        );
        for chunk in bytes.chunks(MAX_STREAM_FRAME_BYTES) {
            let _ = uploads.on_frame(&data_frame(channel, chunk));
        }
        let ack = uploads
            .on_frame(&close_frame(channel, None))
            .expect("an acknowledgement");
        assert_eq!(ack.kind, FrameKind::Close);
        assert!(ack.payload.is_empty(), "{:?}", ack.payload);
    }

    #[test]
    fn an_upload_is_collected_acknowledged_and_consumed_once() {
        let uploads = Uploads::default();
        let bytes: Vec<u8> = (0..(MAX_STREAM_FRAME_BYTES * CREDIT_REFILL as usize + 3))
            .map(|i| (i % 7) as u8)
            .collect();
        assert!(
            uploads
                .on_frame(&open_frame(1, "u1", bytes.len() as u64))
                .is_none()
        );
        let mut credits = 0;
        for chunk in bytes.chunks(MAX_STREAM_FRAME_BYTES) {
            if let Some(frame) = uploads.on_frame(&data_frame(1, chunk)) {
                assert_eq!(frame.kind, FrameKind::Credit);
                credits += 1;
            }
        }
        assert_eq!(credits, 1);
        assert_eq!(uploads.in_flight_count(), 1);
        let ack = uploads.on_frame(&close_frame(1, None)).unwrap();
        assert!(ack.payload.is_empty());
        assert_eq!(uploads.in_flight_count(), 0);
        assert_eq!(uploads.completed_count(), 1);

        let taken = uploads.take_all(&["u1".to_string()]).unwrap();
        assert_eq!(taken[0].bytes, bytes);
        assert_eq!(taken[0].mime, "image/png");
        assert!(taken[0].data_url().starts_with("data:image/png;base64,"));
        assert_eq!(uploads.completed_count(), 0);
        assert!(uploads.take_all(&["u1".to_string()]).is_err(), "consumed");
    }

    #[test]
    fn opens_are_validated_and_limited() {
        let uploads = Uploads::default();
        assert!(
            refusal(uploads.on_frame(&open_frame(1, "big", MAX_UPLOAD_BYTES as u64 + 1)))
                .contains("byte limit")
        );
        assert!(refusal(uploads.on_frame(&open_frame(1, "", 1))).contains("id"));
        assert!(refusal(uploads.on_frame(&open_frame(1, "with space", 1))).contains("id"));
        let mut bad_mime = StreamOpen {
            purpose: UPLOAD_PURPOSE.into(),
            request_id: None,
            upload_id: Some("m".into()),
            mime: Some("image/png;base64,AAAA".into()),
            len: Some(1),
        };
        let frame = |open: &StreamOpen| Frame {
            channel: 1,
            kind: FrameKind::Open,
            payload: Bytes::from(serde_json::to_vec(open).unwrap()),
        };
        assert!(refusal(uploads.on_frame(&frame(&bad_mime))).contains("media type"));
        bad_mime.mime = Some("image/png".into());
        bad_mime.len = None;
        assert!(refusal(uploads.on_frame(&frame(&bad_mime))).contains("length"));
        bad_mime.len = Some(1);
        bad_mime.purpose = "attachment".into();
        assert!(refusal(uploads.on_frame(&frame(&bad_mime))).contains("cannot open"));
        assert!(
            refusal(uploads.on_frame(&Frame {
                channel: 1,
                kind: FrameKind::Open,
                payload: Bytes::from_static(b"nope"),
            }))
            .contains("bad stream open")
        );

        for index in 0..MAX_UPLOADS_IN_FLIGHT {
            assert!(
                uploads
                    .on_frame(&open_frame(1 + 2 * index as u16, &format!("f{index}"), 10))
                    .is_none()
            );
        }
        assert!(refusal(uploads.on_frame(&open_frame(99, "one-more", 10))).contains("in flight"));
        assert!(refusal(uploads.on_frame(&open_frame(101, "f0", 10))).contains("in flight"));
        // A refused stream mid-way frees its slot.
        assert!(refusal(uploads.on_frame(&data_frame(1, &[0; 11]))).contains("declared"));
        assert_eq!(uploads.in_flight_count(), MAX_UPLOADS_IN_FLIGHT - 1);
        assert!(refusal(uploads.on_frame(&open_frame(101, "f1", 10))).contains("already exists"));
        assert!(uploads.on_frame(&open_frame(101, "f0", 10)).is_none());
        // A short stream is an error, not a completed upload.
        assert!(refusal(uploads.on_frame(&close_frame(3, None))).contains("of the 10"));
        assert_eq!(uploads.completed_count(), 0);
        // Late frames on a dropped channel draw no answer.
        assert!(uploads.on_frame(&data_frame(3, b"x")).is_none());
        assert!(uploads.on_frame(&close_frame(3, None)).is_none());
    }

    #[test]
    fn completed_uploads_are_capped_and_taken_all_or_none() {
        let uploads = Uploads::default();
        for index in 0..(MAX_COMPLETED_UPLOADS + 2) {
            upload(&uploads, 1, &format!("c{index}"), b"abc");
        }
        assert_eq!(uploads.completed_count(), MAX_COMPLETED_UPLOADS);
        assert!(
            uploads.take_all(&["c0".to_string()]).is_err(),
            "the oldest were dropped"
        );
        assert!(uploads.take_all(&["c1".to_string()]).is_err());
        let error = uploads
            .take_all(&["c2".to_string(), "nope".to_string()])
            .unwrap_err();
        assert!(error.contains("nope"), "{error}");
        assert_eq!(
            uploads.completed_count(),
            MAX_COMPLETED_UPLOADS,
            "a failed take consumes nothing"
        );
        assert!(
            uploads
                .take_all(&["c2".to_string(), "c2".to_string()])
                .is_err(),
            "one upload cannot be named twice"
        );
        let taken = uploads
            .take_all(&["c5".to_string(), "c2".to_string()])
            .unwrap();
        assert_eq!(taken.len(), 2);
        assert_eq!(uploads.completed_count(), MAX_COMPLETED_UPLOADS - 2);
        assert!(uploads.take_all(&[]).unwrap().is_empty());
        // A completed id is reserved until it is taken.
        assert!(refusal(uploads.on_frame(&open_frame(1, "c3", 1))).contains("already exists"));
        assert!(uploads.on_frame(&open_frame(1, "c2", 1)).is_none());
    }
}
