use serde::{Deserialize, Serialize};
use tracing::debug;

const MAX_CHUNK_SIZE: usize = 20 * 1024 * 1024; // 20MB max chunk size
const CHUNK_OVERLAP_SECONDS: f64 = 1.0; // 1 second overlap between chunks
const MIN_WAV_HEADER_SIZE: usize = 44; // Minimum size for a valid WAV file header
pub const TINFOIL_MAX_SIZE: usize = 512 * 1024; // 0.5MB max size for Tinfoil provider

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioChunk {
    pub data: Vec<u8>,
    pub start_time: f64,
    pub end_time: f64,
    pub index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionSegment {
    pub text: String,
    pub start: Option<f64>,
    pub end: Option<f64>,
    pub chunk_index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergedTranscription {
    pub text: String,
    pub segments: Option<Vec<TranscriptionSegment>>,
    pub language: Option<String>,
    pub duration: Option<f64>,
}

pub struct AudioSplitter {
    max_chunk_size: usize,
    overlap_seconds: f64,
}

impl Default for AudioSplitter {
    fn default() -> Self {
        Self {
            max_chunk_size: MAX_CHUNK_SIZE,
            overlap_seconds: CHUNK_OVERLAP_SECONDS,
        }
    }
}

impl AudioSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn should_split(&self, audio_data: &[u8]) -> bool {
        audio_data.len() > self.max_chunk_size
    }

    pub fn split_audio(
        &self,
        audio_data: &[u8],
        content_type: &str,
    ) -> Result<Vec<AudioChunk>, String> {
        if !self.should_split(audio_data) {
            // No need to split, return single chunk
            debug!(
                "File size {} bytes is under limit, returning as single chunk",
                audio_data.len()
            );
            return Ok(vec![AudioChunk {
                data: audio_data.to_vec(),
                start_time: 0.0,
                end_time: 0.0,
                index: 0,
            }]);
        }

        debug!(
            "File size {} bytes exceeds limit, splitting audio",
            audio_data.len()
        );

        // For WAV files, use simple WAV splitting
        // For other formats, we'll need to decode and re-encode
        match content_type {
            "audio/wav" | "audio/wave" => self.split_wav(audio_data),
            _ => {
                // For MP3 and other formats return full file as 1 chunk
                debug!(
                    "Non-WAV format ({}), returning entire file as single chunk",
                    content_type
                );
                Ok(vec![AudioChunk {
                    data: audio_data.to_vec(),
                    start_time: 0.0,
                    end_time: 0.0, // Unknown duration
                    index: 0,
                }])
            }
        }
    }

    fn split_wav(&self, audio_data: &[u8]) -> Result<Vec<AudioChunk>, String> {
        // WAV file structure:
        // - RIFF header (12 bytes)
        // - fmt chunk (usually 24 bytes)
        // - data chunk header (8 bytes) + actual audio data

        if audio_data.len() < MIN_WAV_HEADER_SIZE {
            return Err(format!(
                "Invalid WAV file: too small (minimum {} bytes required)",
                MIN_WAV_HEADER_SIZE
            ));
        }

        // Verify it's a WAV file
        if &audio_data[0..4] != b"RIFF" || &audio_data[8..12] != b"WAVE" {
            return Err("Not a valid WAV file".to_string());
        }

        // Find both fmt and data chunks - they can be in any order
        let mut pos = 12;
        let mut data_start = 0;
        let mut data_size = 0;
        let mut fmt_pos = 0;
        let mut fmt_size = 0;

        while pos + 8 <= audio_data.len() {
            let chunk_id = &audio_data[pos..pos + 4];
            let chunk_size = u32::from_le_bytes([
                audio_data[pos + 4],
                audio_data[pos + 5],
                audio_data[pos + 6],
                audio_data[pos + 7],
            ]) as usize;

            if chunk_id == b"fmt " {
                fmt_pos = pos + 8;
                fmt_size = chunk_size;
            } else if chunk_id == b"data" {
                data_start = pos + 8;
                data_size = chunk_size;
            }

            // Stop if we've found both chunks
            if fmt_pos != 0 && data_start != 0 {
                break;
            }

            // Prevent integer overflow and infinite loops
            // Check if chunk_size would cause pos to overflow or exceed file bounds
            let next_pos = match pos.checked_add(8) {
                Some(p) => match p.checked_add(chunk_size) {
                    Some(next) => next,
                    None => {
                        return Err("Invalid WAV file: chunk size would cause overflow".to_string());
                    }
                },
                None => {
                    return Err("Invalid WAV file: position overflow".to_string());
                }
            };

            // Ensure we don't jump beyond the file bounds
            if next_pos > audio_data.len() {
                // Skip unknown chunks that extend beyond file bounds
                // This is common in truncated files
                break;
            }

            pos = next_pos;
            if chunk_size % 2 == 1 {
                // Check for overflow before adding padding byte
                pos = match pos.checked_add(1) {
                    Some(p) => p,
                    None => {
                        return Err("Invalid WAV file: position overflow with padding".to_string());
                    }
                };
            }
        }

        if fmt_pos == 0 {
            return Err("WAV file has no fmt chunk".to_string());
        }

        if data_start == 0 {
            return Err("WAV file has no data chunk".to_string());
        }

        // Validate fmt chunk size and bounds
        if fmt_size < 16 {
            return Err("WAV fmt chunk too small".to_string());
        }
        if fmt_pos + fmt_size > audio_data.len() {
            return Err("WAV fmt chunk exceeds file length".to_string());
        }

        // Validate that the data chunk size doesn't exceed the file length
        if data_start + data_size > audio_data.len() {
            return Err("WAV file data chunk exceeds file length".to_string());
        }

        // Extract header (everything before the data chunk header)
        if data_start < 8 {
            return Err("Invalid WAV file: data chunk offset too small".to_string());
        }
        let data_header_pos = data_start - 8;
        let header = &audio_data[0..data_header_pos];
        let audio_samples = &audio_data[data_start..data_start + data_size];

        // Parse format info from fmt chunk
        let fmt_data = &audio_data[fmt_pos..fmt_pos + fmt_size];
        let channels = u16::from_le_bytes([fmt_data[2], fmt_data[3]]);
        let sample_rate = u32::from_le_bytes([fmt_data[4], fmt_data[5], fmt_data[6], fmt_data[7]]);
        let bytes_per_second =
            u32::from_le_bytes([fmt_data[8], fmt_data[9], fmt_data[10], fmt_data[11]]) as usize;
        let block_align = u16::from_le_bytes([fmt_data[12], fmt_data[13]]) as usize;

        // Validate format parameters to prevent division by zero
        if block_align == 0 {
            return Err("Invalid WAV file: block_align is zero".to_string());
        }
        if bytes_per_second == 0 {
            return Err("Invalid WAV file: bytes_per_second is zero".to_string());
        }

        debug!(
            "WAV file: {} channels, {} Hz, {} bytes/sec",
            channels, sample_rate, bytes_per_second
        );

        // Calculate chunk sizes with checked math and alignment
        let header_overhead = header.len() + 8; // Header and data chunk header
        let available = self
            .max_chunk_size
            .checked_sub(header_overhead)
            .ok_or_else(|| {
                format!(
                    "WAV file header too large for chunking: {} bytes (max chunk size: {} bytes)",
                    header_overhead, self.max_chunk_size
                )
            })?;

        // Align to sample boundaries and ensure at least one frame
        let chunk_duration_bytes = (available / block_align) * block_align;
        if chunk_duration_bytes < block_align {
            return Err(
                "Computed chunk size is smaller than one audio frame; increase max_chunk_size"
                    .to_string(),
            );
        }

        // Calculate overlap, ensuring it doesn't exceed chunk size
        let mut overlap_bytes =
            (((self.overlap_seconds * bytes_per_second as f64).round() as usize) / block_align)
                * block_align;
        if overlap_bytes >= chunk_duration_bytes {
            overlap_bytes = 0; // Disable overlap if it would exceed chunk size
        }

        let mut chunks = Vec::new();
        let mut offset = 0;
        let mut index = 0;

        while offset < audio_samples.len() {
            let start = if index > 0 && offset >= overlap_bytes {
                offset - overlap_bytes
            } else {
                offset
            };

            let end = start
                .saturating_add(chunk_duration_bytes)
                .min(audio_samples.len());

            // Ensure we're making progress to prevent infinite loops
            if end <= offset {
                return Err(
                    "Split produced a non-progressing window; check max_chunk_size/overlap"
                        .to_string(),
                );
            }

            let chunk_samples = &audio_samples[start..end];

            // Create a new WAV file for this chunk
            let chunk_wav = self.create_wav_from_samples(header, chunk_samples)?;

            let start_time = start as f64 / bytes_per_second as f64;
            let end_time = end as f64 / bytes_per_second as f64;

            chunks.push(AudioChunk {
                data: chunk_wav,
                start_time,
                end_time,
                index,
            });

            let chunk_size = chunks.last().map(|c| c.data.len()).unwrap_or(0);
            debug!(
                "Created WAV chunk {} ({:.1}s - {:.1}s), size: {} bytes",
                index, start_time, end_time, chunk_size
            );

            offset = end;
            index += 1;
        }

        Ok(chunks)
    }

    fn create_wav_from_samples(
        &self,
        original_header: &[u8],
        samples: &[u8],
    ) -> Result<Vec<u8>, String> {
        let mut wav = Vec::new();

        // Copy the original header up to the data chunk
        wav.extend_from_slice(original_header);

        // Update the data chunk size
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(samples.len() as u32).to_le_bytes());

        // Add the actual audio samples
        wav.extend_from_slice(samples);

        // Update the RIFF chunk size (file size - 8)
        let riff_size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&riff_size.to_le_bytes());

        Ok(wav)
    }
}

pub fn merge_transcriptions(
    results: Vec<(usize, serde_json::Value)>,
) -> Result<MergedTranscription, String> {
    if results.is_empty() {
        return Err("No transcription results to merge".to_string());
    }

    // Sort results by chunk index
    let mut sorted_results = results;
    sorted_results.sort_by_key(|(index, _)| *index);

    let mut merged_text = String::new();
    let mut all_segments = Vec::new();
    let mut detected_language: Option<String> = None;

    for (chunk_index, result) in sorted_results {
        // Extract text
        if let Some(text) = result.get("text").and_then(|t| t.as_str()) {
            if !merged_text.is_empty() {
                merged_text.push(' ');
            }
            merged_text.push_str(text);
        }

        // Extract language (use first detected)
        if detected_language.is_none() {
            if let Some(lang) = result.get("language").and_then(|l| l.as_str()) {
                detected_language = Some(lang.to_string());
            }
        }

        // Extract segments if available
        if let Some(segments) = result.get("segments").and_then(|s| s.as_array()) {
            for segment in segments {
                if let Some(seg_obj) = segment.as_object() {
                    all_segments.push(TranscriptionSegment {
                        text: seg_obj
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string(),
                        start: seg_obj.get("start").and_then(|s| s.as_f64()),
                        end: seg_obj.get("end").and_then(|e| e.as_f64()),
                        chunk_index,
                    });
                }
            }
        }
    }

    Ok(MergedTranscription {
        text: merged_text,
        segments: if all_segments.is_empty() {
            None
        } else {
            Some(all_segments)
        },
        language: detected_language,
        duration: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_split() {
        let splitter = AudioSplitter::new();

        // Test data smaller than limit
        let small_data = vec![0u8; MAX_CHUNK_SIZE / 2];
        assert!(!splitter.should_split(&small_data));

        // Test data exactly at limit
        let exact_data = vec![0u8; MAX_CHUNK_SIZE];
        assert!(!splitter.should_split(&exact_data));

        // Test data just over limit
        let just_over_data = vec![0u8; MAX_CHUNK_SIZE + 1];
        assert!(splitter.should_split(&just_over_data));

        // Test data significantly over limit
        let large_data = vec![0u8; MAX_CHUNK_SIZE * 2];
        assert!(splitter.should_split(&large_data));
    }

    #[test]
    fn test_split_audio_small_file() {
        let splitter = AudioSplitter::new();

        // Test with small file that doesn't need splitting
        let small_audio = vec![0u8; 1024];
        let result = splitter.split_audio(&small_audio, "audio/mp3").unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].index, 0);
        assert_eq!(result[0].data.len(), 1024);
    }

    #[test]
    fn test_split_audio_non_wav_format() {
        let splitter = AudioSplitter::new();

        // Test with large MP3 file - should return as single chunk
        let large_mp3 = vec![0u8; MAX_CHUNK_SIZE * 2];
        let result = splitter.split_audio(&large_mp3, "audio/mp3").unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].data.len(), MAX_CHUNK_SIZE * 2);
    }

    #[test]
    fn test_split_wav_invalid_file() {
        let splitter = AudioSplitter::new();

        // Test with file too small to be valid WAV
        let tiny_file = vec![0u8; 10];
        let result = splitter.split_wav(&tiny_file);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too small"));

        // Test with file that's large enough but not a WAV
        let not_wav = vec![0u8; 100];
        let result = splitter.split_wav(&not_wav);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Not a valid WAV"));
    }

    #[test]
    fn test_merge_transcriptions() {
        let results = vec![
            (
                0,
                serde_json::json!({
                    "text": "Hello world",
                    "language": "en"
                }),
            ),
            (
                1,
                serde_json::json!({
                    "text": "this is a test",
                }),
            ),
            (
                2,
                serde_json::json!({
                    "text": "of merging transcriptions",
                }),
            ),
        ];

        let merged = merge_transcriptions(results).unwrap();
        assert_eq!(
            merged.text,
            "Hello world this is a test of merging transcriptions"
        );
        assert_eq!(merged.language, Some("en".to_string()));
    }
}
