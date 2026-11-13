//! Lark SDK pbbp2 Frame wire schema (fixed SDK lib/index.js:88496-88527).
//! Kept local: no protobuf runtime or JS dependency is required at runtime.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Frame {
    pub seq: u64,
    pub log: u64,
    pub service: u64,
    pub method: u64,
    pub headers: Vec<(String, String)>,
    pub encoding: Vec<u8>,
    pub payload_type: Vec<u8>,
    pub payload: Vec<u8>,
    pub log_new: Vec<u8>,
}
fn varint(bytes: &mut &[u8]) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = bytes.split_first().ok_or("truncated protobuf varint")?;
        *bytes = rest;
        if shift == 63 && byte > 1 {
            return Err("overflow protobuf varint".into());
        }
        value |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    Err("overflow protobuf varint".into())
}
fn length(bytes: &mut &[u8]) -> Result<Vec<u8>, String> {
    let size = usize::try_from(varint(bytes)?).map_err(|_| "protobuf length overflow")?;
    if size > bytes.len() {
        return Err("truncated protobuf field".into());
    }
    let value = bytes[..size].to_vec();
    *bytes = &bytes[size..];
    Ok(value)
}
fn skip(bytes: &mut &[u8], wire: u64) -> Result<(), String> {
    match wire {
        0 => {
            varint(bytes)?;
        }
        2 => {
            length(bytes)?;
        }
        1 | 5 => {
            let n = if wire == 1 { 8 } else { 4 };
            if bytes.len() < n {
                return Err("truncated protobuf fixed field".into());
            }
            *bytes = &bytes[n..];
        }
        _ => return Err("unsupported protobuf wire type".into()),
    }
    Ok(())
}
fn put_var(out: &mut Vec<u8>, mut n: u64) {
    while n >= 128 {
        out.push((n as u8 & 127) | 128);
        n >>= 7;
    }
    out.push(n as u8);
}
fn put_bytes(out: &mut Vec<u8>, tag: u64, bytes: &[u8]) {
    put_var(out, tag * 8 + 2);
    put_var(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}
impl Frame {
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    pub fn decode(mut bytes: &[u8]) -> Result<Self, String> {
        let mut frame = Self::default();
        let mut required = 0u8;
        while !bytes.is_empty() {
            let tag = varint(&mut bytes)?;
            if (1..=4).contains(&(tag >> 3)) && tag & 7 == 0 {
                required |= 1u8 << ((tag >> 3) - 1);
            }
            match (tag >> 3, tag & 7) {
                (1, 0) => frame.seq = varint(&mut bytes)?,
                (2, 0) => frame.log = varint(&mut bytes)?,
                (3, 0) => frame.service = varint(&mut bytes)?,
                (4, 0) => frame.method = varint(&mut bytes)?,
                (5, 2) => {
                    let header = length(&mut bytes)?;
                    let mut h = header.as_slice();
                    let (mut key, mut value) = (String::new(), String::new());
                    while !h.is_empty() {
                        let tag = varint(&mut h)?;
                        match (tag >> 3, tag & 7) {
                            (1, 2) => {
                                key =
                                    String::from_utf8(length(&mut h)?).map_err(|e| e.to_string())?
                            }
                            (2, 2) => {
                                value =
                                    String::from_utf8(length(&mut h)?).map_err(|e| e.to_string())?
                            }
                            (_, wire) => skip(&mut h, wire)?,
                        }
                    }
                    frame.headers.push((key, value));
                }
                (6, 2) => frame.encoding = length(&mut bytes)?,
                (7, 2) => frame.payload_type = length(&mut bytes)?,
                (8, 2) => frame.payload = length(&mut bytes)?,
                (9, 2) => frame.log_new = length(&mut bytes)?,
                (_, wire) => skip(&mut bytes, wire)?,
            }
        }
        if required != 15 {
            return Err("missing required pbbp2 Frame field".into());
        }
        Ok(frame)
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (tag, n) in [
            (1, self.seq),
            (2, self.log),
            (3, self.service),
            (4, self.method),
        ] {
            put_var(&mut out, tag * 8);
            put_var(&mut out, n);
        }
        for (key, value) in &self.headers {
            let mut h = Vec::new();
            put_bytes(&mut h, 1, key.as_bytes());
            put_bytes(&mut h, 2, value.as_bytes());
            put_bytes(&mut out, 5, &h);
        }
        for (tag, bytes) in [
            (6, &self.encoding),
            (7, &self.payload_type),
            (8, &self.payload),
            (9, &self.log_new),
        ] {
            if !bytes.is_empty() {
                put_bytes(&mut out, tag, bytes);
            }
        }
        out
    }
}
