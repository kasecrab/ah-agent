//! A 44-byte RIFF header in front of the samples. Nothing is written to disk:
//! this exists only to hand the provider something it will accept.

pub fn encode(pcm: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + pcm.len() * 2);
    let byte_rate = rate * 2;
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_describes_the_samples() {
        let w = encode(&[1, -1, 2], 16_000);
        assert_eq!(w.len(), 44 + 6);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(w[4..8].try_into().unwrap()), 42);
        assert_eq!(&w[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes(w[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(w[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(w[28..32].try_into().unwrap()), 32_000);
        assert_eq!(&w[36..40], b"data");
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 6);
        assert_eq!(i16::from_le_bytes(w[44..46].try_into().unwrap()), 1);
        assert_eq!(i16::from_le_bytes(w[46..48].try_into().unwrap()), -1);
    }

    #[test]
    fn an_empty_clip_is_still_a_valid_file() {
        let w = encode(&[], 48_000);
        assert_eq!(w.len(), 44);
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 0);
    }
}
