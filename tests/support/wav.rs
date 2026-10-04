//! Frame-index WAV fixtures (M10 §9): every frame encodes its own index, so a
//! render log says exactly which frames played, and in what order.

/// The harness device's rate, so nothing is resampled between the file and
/// the render log.
pub const RATE: u32 = 48_000;

/// Small on purpose: both channels stay far below the i16 range, so a frame
/// decodes back exactly whether Symphonia scales by 32767 or 32768.
const BASE: u32 = 1024;

/// A stereo, 16-bit PCM WAV of `frames` frames at [`RATE`]. Frame `i` holds
/// `i / BASE` on the left and `i % BASE + 1` on the right. The right channel
/// is never zero, so no frame equals silence and dropping silence from a
/// render log can never hide a frame.
pub fn frame_index_wav(frames: u32) -> Vec<u8> {
    let data_len = frames * 4;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&2u16.to_le_bytes()); // channels
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 4).to_le_bytes()); // byte rate
    wav.extend_from_slice(&4u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for index in 0..frames {
        let left = (index / BASE) as i16;
        let right = (index % BASE + 1) as i16;
        wav.extend_from_slice(&left.to_le_bytes());
        wav.extend_from_slice(&right.to_le_bytes());
    }
    wav
}

/// Decodes an interleaved stereo render log back to frame indices, dropping
/// silence (both channels zero), which no encoded frame ever is.
pub fn frame_indices(samples: &[f32]) -> Vec<u32> {
    samples
        .as_chunks::<2>()
        .0
        .iter()
        .filter(|frame| frame[0] != 0.0 || frame[1] != 0.0)
        .map(|frame| {
            let left = (frame[0] * 32768.0).round() as u32;
            let right = (frame[1] * 32768.0).round() as u32;
            left * BASE + right - 1
        })
        .collect()
}
