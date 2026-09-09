#![forbid(unsafe_code)]

use std::{fmt, io::{self, BufRead, Read}};

use serde_json::Value;

pub const DEFAULT_MAX_METADATA_LINE_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_DATA_CHUNK_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub enum ReceiverError {
    Io(io::Error),
    MetadataTooLarge,
    InvalidMetadata,
    MissingSchemaVersion,
    InvalidByteLength,
    DataChunkTooLarge { requested: u64, maximum: usize },
    TruncatedData { expected: usize, received: usize },
}

impl fmt::Display for ReceiverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "receiver I/O failed: {error}"),
            Self::MetadataTooLarge => write!(f, "receiver metadata line exceeds the configured bound"),
            Self::InvalidMetadata => write!(f, "receiver metadata is not a JSON object"),
            Self::MissingSchemaVersion => write!(f, "receiver metadata is missing schemaVersion"),
            Self::InvalidByteLength => write!(f, "receiver metadata byteLength must be a non-negative integer"),
            Self::DataChunkTooLarge { requested, maximum } => write!(
                f,
                "receiver data chunk requests {requested} bytes, above the {maximum}-byte bound"
            ),
            Self::TruncatedData { expected, received } => write!(
                f,
                "receiver data stream closed after {received} of {expected} expected bytes"
            ),
        }
    }
}

impl std::error::Error for ReceiverError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ReceiverError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReceiverFrame {
    pub metadata: Value,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiverLimits {
    pub max_metadata_line_bytes: usize,
    pub max_data_chunk_bytes: usize,
}

impl Default for ReceiverLimits {
    fn default() -> Self {
        Self {
            max_metadata_line_bytes: DEFAULT_MAX_METADATA_LINE_BYTES,
            max_data_chunk_bytes: DEFAULT_MAX_DATA_CHUNK_BYTES,
        }
    }
}

fn read_bounded_line(
    reader: &mut impl BufRead,
    maximum: usize,
) -> Result<Option<Vec<u8>>, ReceiverError> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if line.is_empty() { Ok(None) } else { Ok(Some(line)) };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(take) > maximum {
            return Err(ReceiverError::MetadataTooLarge);
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

fn parse_byte_length(metadata: &Value, maximum: usize) -> Result<usize, ReceiverError> {
    let requested = metadata
        .get("byteLength")
        .and_then(Value::as_u64)
        .ok_or(ReceiverError::InvalidByteLength)?;
    if requested > maximum as u64 {
        return Err(ReceiverError::DataChunkTooLarge { requested, maximum });
    }
    usize::try_from(requested).map_err(|_| ReceiverError::InvalidByteLength)
}

pub fn receive_one(
    metadata_reader: &mut impl BufRead,
    data_reader: &mut impl Read,
    limits: ReceiverLimits,
) -> Result<Option<ReceiverFrame>, ReceiverError> {
    let Some(line) = read_bounded_line(metadata_reader, limits.max_metadata_line_bytes)? else {
        return Ok(None);
    };
    let metadata: Value = serde_json::from_slice(&line).map_err(|_| ReceiverError::InvalidMetadata)?;
    let object = metadata.as_object().ok_or(ReceiverError::InvalidMetadata)?;
    if !object
        .get("schemaVersion")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty() && value.len() <= 128)
    {
        return Err(ReceiverError::MissingSchemaVersion);
    }

    let byte_length = parse_byte_length(&metadata, limits.max_data_chunk_bytes)?;
    let mut data = vec![0_u8; byte_length];
    let mut received = 0;
    while received < byte_length {
        match data_reader.read(&mut data[received..])? {
            0 => {
                return Err(ReceiverError::TruncatedData {
                    expected: byte_length,
                    received,
                })
            }
            count => received += count,
        }
    }
    Ok(Some(ReceiverFrame { metadata, data }))
}

pub fn receive_all(
    metadata_reader: &mut impl BufRead,
    data_reader: &mut impl Read,
    limits: ReceiverLimits,
    mut sink: impl FnMut(ReceiverFrame) -> Result<(), ReceiverError>,
) -> Result<u64, ReceiverError> {
    let mut count = 0_u64;
    while let Some(frame) = receive_one(metadata_reader, data_reader, limits)? {
        sink(frame)?;
        count = count.saturating_add(1);
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};

    use super::*;

    fn metadata(byte_length: usize, sequence: usize) -> String {
        format!(
            "{{\"schemaVersion\":\"gha-indie-worker.build-log-metadata/v1\",\"event\":\"chunk\",\"jobId\":\"build-1\",\"stream\":\"stdout\",\"sequence\":{sequence},\"byteLength\":{byte_length},\"timestamp\":\"2026-09-09T19:45:00Z\"}}\n"
        )
    }

    #[test]
    fn reads_exact_raw_bytes_for_each_metadata_frame() {
        let mut metadata_input = BufReader::new(Cursor::new(format!(
            "{}{}",
            metadata(5, 1),
            metadata(4, 2)
        )));
        let mut data_input = Cursor::new(b"helloRust".to_vec());
        let first = receive_one(&mut metadata_input, &mut data_input, ReceiverLimits::default())
            .unwrap()
            .unwrap();
        let second = receive_one(&mut metadata_input, &mut data_input, ReceiverLimits::default())
            .unwrap()
            .unwrap();
        assert_eq!(first.data, b"hello");
        assert_eq!(second.data, b"Rust");
        assert_eq!(first.metadata["sequence"], 1);
        assert_eq!(second.metadata["sequence"], 2);
    }

    #[test]
    fn zero_length_lifecycle_frame_does_not_consume_data() {
        let line = "{\"schemaVersion\":\"gha-indie-worker.build-log-metadata/v1\",\"event\":\"stream_closed\",\"jobId\":\"build-1\",\"stream\":\"stdout\",\"sequence\":2,\"byteLength\":0,\"timestamp\":\"2026-09-09T19:45:00Z\"}\n";
        let mut metadata_input = BufReader::new(Cursor::new(line.as_bytes()));
        let mut data_input = Cursor::new(b"untouched".to_vec());
        let frame = receive_one(&mut metadata_input, &mut data_input, ReceiverLimits::default())
            .unwrap()
            .unwrap();
        assert!(frame.data.is_empty());
        assert_eq!(data_input.position(), 0);
    }

    #[test]
    fn rejects_oversized_chunk_before_reading_data() {
        let mut metadata_input = BufReader::new(Cursor::new(metadata(17, 1)));
        let mut data_input = Cursor::new(b"secret-never-read".to_vec());
        let result = receive_one(
            &mut metadata_input,
            &mut data_input,
            ReceiverLimits {
                max_metadata_line_bytes: 4096,
                max_data_chunk_bytes: 16,
            },
        );
        assert!(matches!(result, Err(ReceiverError::DataChunkTooLarge { .. })));
        assert_eq!(data_input.position(), 0);
    }

    #[test]
    fn reports_truncated_data_without_fabricating_bytes() {
        let mut metadata_input = BufReader::new(Cursor::new(metadata(8, 1)));
        let mut data_input = Cursor::new(b"short".to_vec());
        let result = receive_one(&mut metadata_input, &mut data_input, ReceiverLimits::default());
        assert!(matches!(
            result,
            Err(ReceiverError::TruncatedData {
                expected: 8,
                received: 5
            })
        ));
    }

    #[test]
    fn metadata_line_bound_is_enforced() {
        let line = format!(
            "{{\"schemaVersion\":\"{}\",\"byteLength\":0}}\n",
            "x".repeat(256)
        );
        let mut metadata_input = BufReader::new(Cursor::new(line));
        let mut data_input = Cursor::new(Vec::<u8>::new());
        let result = receive_one(
            &mut metadata_input,
            &mut data_input,
            ReceiverLimits {
                max_metadata_line_bytes: 64,
                max_data_chunk_bytes: 64,
            },
        );
        assert!(matches!(result, Err(ReceiverError::MetadataTooLarge)));
    }
}
