//! WebSocket frame decoding for the native listener, and TS-shaped server frames.
use bytes::BytesMut;

pub(crate) const MAX_CLIENT_HEADER_BYTES: usize = 14;
pub(crate) const MAX_SERVER_HEADER_BYTES: usize = 10;

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

#[derive(Debug)]
pub(crate) struct Frame {
    pub opcode: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DecodeError {
    Unmasked,
    TooLarge,
}

#[derive(Default)]
pub(crate) struct Decoder {
    pub buffer: BytesMut,
}

impl Decoder {
    #[cfg(test)]
    pub fn next(&mut self) -> Result<Option<Frame>, DecodeError> {
        self.decode(true, MAX_SAFE_INTEGER)
    }

    /// The native listener's bound, checked from the header before payload
    /// allocation.
    pub fn next_limited(&mut self, max_payload: u64) -> Result<Option<Frame>, DecodeError> {
        let max_payload = if crate::mutants::on("native-ws-input-boundary") {
            max_payload.saturating_sub(1)
        } else {
            max_payload
        };
        self.decode(true, max_payload)
    }

    #[cfg(test)]
    pub fn next_server(&mut self) -> Result<Option<Frame>, DecodeError> {
        self.decode(false, MAX_SAFE_INTEGER)
    }

    fn decode(
        &mut self,
        require_mask: bool,
        max_payload: u64,
    ) -> Result<Option<Frame>, DecodeError> {
        let buf = &self.buffer;
        if buf.len() < 2 {
            return Ok(None);
        }
        let (size, offset) = match buf[1] & 0x7f {
            126 => {
                if buf.len() < 4 {
                    return Ok(None);
                }
                (u64::from(u16::from_be_bytes([buf[2], buf[3]])), 4)
            }
            127 => {
                if buf.len() < 10 {
                    return Ok(None);
                }
                let size = u64::from_be_bytes(buf[2..10].try_into().expect("eight bytes"));
                if size > MAX_SAFE_INTEGER {
                    return Err(DecodeError::TooLarge);
                }
                (size, 10)
            }
            size => (u64::from(size), 2),
        };
        if size > max_payload {
            return Err(DecodeError::TooLarge);
        }
        // TS checks masking after the extended length, before awaiting payload.
        let masked = buf[1] & 0x80 != 0;
        if require_mask && !masked {
            return Err(DecodeError::Unmasked);
        }
        let size = usize::try_from(size).map_err(|_| DecodeError::TooLarge)?;
        let payload_start = offset + if masked { 4 } else { 0 };
        let total = size
            .checked_add(payload_start)
            .ok_or(DecodeError::TooLarge)?;
        if buf.len() < total {
            return Ok(None);
        }
        let wire = self.buffer.split_to(total).freeze();
        let mut payload = wire[payload_start..].to_vec();
        if masked {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= wire[offset + index % 4];
            }
        }
        Ok(Some(Frame {
            opcode: wire[0] & 15,
            payload,
        }))
    }
}

/// TS-generated replies always use FIN=1, RSV=0, no mask, minimal length.
pub(crate) fn server_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut wire = vec![0x80 | opcode];
    match payload.len() {
        0..126 => wire.push(payload.len() as u8),
        126..=65535 => {
            wire.push(126);
            wire.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        _ => {
            wire.push(127);
            wire.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
    }
    if crate::mutants::on("native-ws-server-header-budget") {
        wire.push(0);
    }
    wire.extend_from_slice(payload);
    wire
}

#[cfg(test)]
pub(crate) fn client_frame(first: u8, length_code: u8, payload: &[u8]) -> Vec<u8> {
    let mut wire = vec![first, 0x80 | length_code];
    match length_code {
        126 => wire.extend_from_slice(&(payload.len() as u16).to_be_bytes()),
        127 => wire.extend_from_slice(&(payload.len() as u64).to_be_bytes()),
        _ => assert_eq!(usize::from(length_code), payload.len()),
    }
    if crate::mutants::on("native-ws-client-header-budget") {
        wire.push(0);
    }
    let mask = [1, 2, 3, 4];
    wire.extend_from_slice(&mask);
    wire.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    wire
}

// Moved from the removed transparent-proxy pairing tests: they pin the
// shared decoder (the native listener's `next_limited` runs the same
// `decode`) and the TS-shaped server frame header.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_waits_for_complete_extended_header_mask_and_payload() {
        let wire = client_frame(0xf1, 127, b"abc");
        let mut decoder = Decoder::default();
        for (index, byte) in wire.iter().enumerate() {
            decoder.buffer.extend_from_slice(&[*byte]);
            let frame = decoder.next().unwrap();
            if index + 1 < wire.len() {
                assert!(frame.is_none());
            } else {
                let frame = frame.unwrap();
                assert_eq!(frame.opcode, 1);
                assert_eq!(frame.payload, b"abc");
            }
        }
        assert!(decoder.buffer.is_empty());
    }

    #[test]
    fn decoder_length_precedes_mask_validation_and_safe_integer_bound_is_exact() {
        let mut decoder = Decoder::default();
        decoder.buffer.extend_from_slice(&[0x81, 127]);
        assert!(decoder.next().unwrap().is_none());
        decoder
            .buffer
            .extend_from_slice(&((1u64 << 53) - 1).to_be_bytes());
        assert_eq!(decoder.next().unwrap_err(), DecodeError::Unmasked);
        decoder.buffer[1] = 255;
        assert!(decoder.next().unwrap().is_none()); // No huge allocation.
        decoder.buffer[2..10].copy_from_slice(&(1u64 << 53).to_be_bytes());
        assert_eq!(decoder.next().unwrap_err(), DecodeError::TooLarge);
    }

    #[test]
    fn server_reply_headers_match_ts_length_boundaries() {
        for (size, expected) in [
            (0, vec![0x8a, 0]),
            (125, vec![0x8a, 125]),
            (126, vec![0x8a, 126, 0, 126]),
            (65535, vec![0x8a, 126, 255, 255]),
            (65536, vec![0x8a, 127, 0, 0, 0, 0, 0, 1, 0, 0]),
        ] {
            let payload = vec![42; size];
            let wire = server_frame(10, &payload);
            assert_eq!(&wire[..expected.len()], expected);
            assert_eq!(&wire[expected.len()..], payload);
        }
    }
}
