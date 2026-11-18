//! PilotDeck's incremental framing: split on LF-LF, then parse each data line.
#[derive(Default)]
pub(crate) struct SseDecoder {
    pending: Vec<u8>,
    started: bool,
}

impl SseDecoder {
    pub fn buffered_len(&self) -> usize {
        self.pending.len()
    }
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, String> {
        self.pending.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some(index) = self.pending.windows(2).position(|pair| pair == b"\n\n") {
            let mut chunk: Vec<u8> = self.pending.drain(..index + 2).collect();
            chunk.truncate(index);
            self.chunk(&chunk, &mut frames);
        }
        Ok(frames)
    }
    pub fn finish(&mut self) -> Result<Vec<String>, String> {
        let mut frames = Vec::new();
        let chunk = std::mem::take(&mut self.pending);
        self.chunk(&chunk, &mut frames);
        Ok(frames)
    }
    fn chunk(&mut self, chunk: &[u8], frames: &mut Vec<String>) {
        let chunk = if self.started {
            chunk
        } else {
            self.started = true;
            chunk.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(chunk)
        };
        let chunk = String::from_utf8_lossy(chunk);
        for line in chunk.split('\n') {
            if let Some(data) = line.strip_prefix("data:") {
                let data = pilotdeck_model_core::codec::metadata::js_trim(data);
                if !data.is_empty() {
                    frames.push(data.to_owned());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn follows_baseline_per_data_line_and_flushes_crlf_at_eof() {
        let mut decoder = SseDecoder::default();
        let bytes = "data: 你\r\ndata: two\r\n\r\ndata: [DONE]\n\n".as_bytes();
        let mut events = Vec::new();
        for byte in bytes {
            events.extend(decoder.push(&[*byte]).unwrap());
        }
        events.extend(decoder.finish().unwrap());
        assert_eq!(events, ["你", "two", "[DONE]"]);
    }
    #[test]
    fn flushes_unterminated_last_event() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: {\"a\":1}").unwrap().is_empty());
        assert_eq!(decoder.finish().unwrap(), ["{\"a\":1}"]);
    }
}
