//! A recording made ready to send: smaller, evenly loud, and wrapped as a
//! WAV file.

use alloc::vec::Vec;

/// One sample is kept for this many recorded. Speech survives it, and the
/// upload, which is the slow part on a PSP, shrinks by the same factor.
const KEEP_ONE_IN: usize = 3;
const LOUD: i32 = 24_000;
/// Below this the recording is treated as this loud, so silence is not
/// turned up into a roar.
const QUIETEST: i32 = 600;

/// The level only one sample in two hundred goes above. The very loudest
/// sample is no guide: a button click next to the microphone reaches full
/// scale and would leave the speech itself quiet.
fn level(samples: &[i16]) -> i32 {
    let mut counts = [0usize; 256];
    for sample in samples {
        counts[(sample.unsigned_abs() >> 7) as usize] += 1;
    }
    let mut above = samples.len() / 200;
    for (step, &count) in counts.iter().enumerate().rev() {
        if count > above {
            return ((step as i32 + 1) << 7).max(QUIETEST);
        }
        above -= count;
    }
    QUIETEST
}

/// `take`, recorded at `rate` samples a second, as a WAV file.
pub fn voice_note(take: &[i16], rate: u32) -> Vec<u8> {
    let level = level(take);
    let rate = rate / KEEP_ONE_IN as u32;
    let bytes = (take.len() / KEEP_ONE_IN * 2) as u32;
    let mut file = Vec::with_capacity(44 + bytes as usize);
    file.extend_from_slice(b"RIFF");
    file.extend_from_slice(&(36 + bytes).to_le_bytes());
    file.extend_from_slice(b"WAVEfmt ");
    // Format size, then PCM with one channel, the rates, and 16-bit samples of 2 bytes.
    for field in [16, 0x0001_0001, rate, rate * 2, 0x0010_0002] {
        file.extend_from_slice(&field.to_le_bytes());
    }
    file.extend_from_slice(b"data");
    file.extend_from_slice(&bytes.to_le_bytes());
    for group in take.chunks_exact(KEEP_ONE_IN) {
        // The average is what stops the higher pitches folding down into noise.
        let average = group.iter().map(|&sample| sample as i32).sum::<i32>() / KEEP_ONE_IN as i32;
        file.extend_from_slice(&((average * LOUD / level).clamp(-32_768, 32_767) as i16).to_le_bytes());
    }
    file
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(file: &[u8]) -> Vec<i16> {
        file[44..].chunks(2).map(|pair| i16::from_le_bytes([pair[0], pair[1]])).collect()
    }

    #[test]
    fn the_header_describes_what_follows() {
        let file = voice_note(&[1000; 22_050], 22_050);
        assert_eq!(&file[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(file[4..8].try_into().unwrap()) as usize, file.len() - 8);
        assert_eq!(u32::from_le_bytes(file[24..28].try_into().unwrap()), 7350);
        assert_eq!(u32::from_le_bytes(file[40..44].try_into().unwrap()) as usize, file.len() - 44);
        assert_eq!(samples(&file).len(), 7350);
    }

    #[test]
    fn a_click_does_not_set_the_loudness() {
        let mut take = [2000i16; 6000];
        take[10] = 32_767;
        let quiet_speech = samples(&voice_note(&take, 22_050));
        assert!(quiet_speech[1000] > 20_000, "speech was left at {}", quiet_speech[1000]);
    }

    #[test]
    fn loud_speech_is_brought_to_the_same_level_as_quiet_speech() {
        let loud = samples(&voice_note(&[30_000; 6000], 22_050));
        let quiet = samples(&voice_note(&[3_000; 6000], 22_050));
        assert!((23_000..=24_500).contains(&loud[100]), "{}", loud[100]);
        assert!((23_000..=24_500).contains(&quiet[100]), "{}", quiet[100]);
    }

    #[test]
    fn a_sample_above_the_level_is_held_at_full_scale() {
        let mut take = [2000i16; 6000];
        take[9..12].fill(32_767);
        assert_eq!(samples(&voice_note(&take, 22_050))[3], 32_767);
    }

    #[test]
    fn silence_stays_quiet() {
        let hiss: Vec<i16> = (0..6000).map(|i| (i % 7) as i16 - 3).collect();
        assert!(samples(&voice_note(&hiss, 22_050)).iter().all(|&sample| sample.abs() < 200));
    }

    #[test]
    fn nothing_recorded_is_an_empty_file() {
        assert_eq!(voice_note(&[], 22_050).len(), 44);
    }
}
