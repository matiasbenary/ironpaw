//! The audio facts that need no hardware: the sample rate, the volume steps, WAV headers and
//! silence trimming. `audio.rs` re-exports them; the host simulator includes this file too.

/// Sample rate of the mic and the speaker, in Hz.
pub const RATE: u32 = 16_000;

/// Frames quieter than this (RMS, dBFS) count as silence when trimming a recording.
// ponytail: fixed threshold. If quiet speakers get clipped, lower it or measure the noise floor.
const SILENCE_DBFS: f32 = -45.0;
/// 20 ms frames of mono s16le at `RATE`.
const FRAME: usize = RATE as usize / 50 * 2;
/// Silence kept around the speech, so the first and last words aren't cut.
const MARGIN_FRAMES: usize = 15;

/// Byte range of `pcm` (mono s16le) without the silence before and after the speech.
/// All silence: the whole thing, and the transcriber decides.
pub fn trim_silence(pcm: &[u8]) -> std::ops::Range<usize> {
    let loud: Vec<bool> = pcm
        .chunks(FRAME)
        .map(|frame| {
            let (sum, n) = frame.chunks_exact(2).fold((0f32, 0f32), |(sum, n), s| {
                let s = i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0;
                (sum + s * s, n + 1.0)
            });
            n > 0.0 && 20.0 * (sum / n).sqrt().max(1e-6).log10() > SILENCE_DBFS
        })
        .collect();
    let (Some(first), Some(last)) = (loud.iter().position(|&l| l), loud.iter().rposition(|&l| l)) else {
        return 0..pcm.len();
    };
    let start = first.saturating_sub(MARGIN_FRAMES) * FRAME;
    let end = ((last + 1 + MARGIN_FRAMES) * FRAME).min(pcm.len());
    start..end
}

/// 44-byte WAV header for 16-bit mono PCM at `RATE`.
pub fn wav_header(data_len: usize) -> [u8; 44] {
    let mut h = [0u8; 44];
    let mut put = |at: usize, bytes: &[u8]| h[at..at + bytes.len()].copy_from_slice(bytes);
    put(0, b"RIFF");
    put(4, &(36 + data_len as u32).to_le_bytes());
    put(8, b"WAVEfmt ");
    put(16, &16u32.to_le_bytes()); // fmt chunk size
    put(20, &1u16.to_le_bytes()); // PCM
    put(22, &1u16.to_le_bytes()); // mono
    put(24, &RATE.to_le_bytes());
    put(28, &(RATE * 2).to_le_bytes()); // bytes per second
    put(32, &2u16.to_le_bytes()); // bytes per sample
    put(34, &16u16.to_le_bytes()); // bits per sample
    put(36, b"data");
    put(40, &(data_len as u32).to_le_bytes());
    h
}
