//! Decoding audio files into memory. Runs on a background thread, never
//! the audio thread: it does file I/O and allocates freely.

use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub type LoadError = Box<dyn std::error::Error + Send + Sync>;

/// Decoded audio, planar f32 at the file's own sample rate.
/// Shared immutably via `Arc`, so any number of players can read it
/// without copying.
pub struct AudioData {
    pub name: String,
    /// Where it came from on disk, if anywhere: what a project file saves.
    pub path: Option<std::path::PathBuf>,
    pub sample_rate: f64,
    pub channels: Vec<Vec<f32>>,
}

/// A summary, not the samples: a derived `Debug` would print millions of
/// numbers.
impl std::fmt::Debug for AudioData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioData")
            .field("name", &self.name)
            .field("path", &self.path)
            .field("sample_rate", &self.sample_rate)
            .field("frames", &self.frames())
            .field("channels", &self.channels.len())
            .finish()
    }
}

impl AudioData {
    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, |c| c.len())
    }

    pub fn duration_secs(&self) -> f64 {
        self.frames() as f64 / self.sample_rate
    }
}

pub fn load_audio(path: &Path) -> Result<AudioData, LoadError> {
    let file = std::fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    // The extension helps symphonia guess the format faster.
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("no audio track found")?;
    let track_id = track.id;
    let sample_rate = track.codec_params.sample_rate.ok_or("unknown sample rate")? as f64;

    let mut decoder =
        symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;

    let mut channels: Vec<Vec<f32>> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            // Symphonia signals end-of-stream as an UnexpectedEof I/O error.
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                let frames = decoded.frames();
                let n_channels = spec.channels.count();
                if channels.is_empty() {
                    channels = vec![Vec::new(); n_channels];
                }

                // Convert whatever the file's sample format is to f32,
                // laid out planar: [ch0 frames..., ch1 frames..., ...]
                let mut samples = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                samples.copy_planar_ref(decoded);
                let data = samples.samples();
                for (ch, out) in channels.iter_mut().enumerate() {
                    out.extend_from_slice(&data[ch * frames..(ch + 1) * frames]);
                }
            }
            // A corrupt packet: skip it rather than failing the whole file.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }

    if channels.is_empty() || channels[0].is_empty() {
        return Err("file contains no audio".into());
    }

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "untitled".into());

    Ok(AudioData { name, path: Some(path.to_path_buf()), sample_rate, channels })
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Writes a minimal 16-bit PCM WAV. Used by tests here and in the sampler.
    pub fn write_wav(path: &Path, sample_rate: u32, channels: &[Vec<i16>]) {
        let n_ch = channels.len() as u16;
        let frames = channels[0].len() as u32;
        let data_len = frames * n_ch as u32 * 2;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_len).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // PCM
        b.extend_from_slice(&n_ch.to_le_bytes());
        b.extend_from_slice(&sample_rate.to_le_bytes());
        b.extend_from_slice(&(sample_rate * n_ch as u32 * 2).to_le_bytes());
        b.extend_from_slice(&(n_ch * 2).to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_len.to_le_bytes());
        for f in 0..frames as usize {
            for ch in channels {
                b.extend_from_slice(&ch[f].to_le_bytes());
            }
        }
        std::fs::write(path, b).unwrap();
    }

    #[test]
    fn loads_stereo_wav() {
        let path = std::env::temp_dir().join("daw_loader_test.wav");
        let left: Vec<i16> = (0..1000).map(|i| (i * 10) as i16).collect();
        let right: Vec<i16> = (0..1000).map(|i| -(i * 10) as i16).collect();
        write_wav(&path, 44_100, &[left, right]);

        let audio = load_audio(&path).unwrap();
        assert_eq!(audio.sample_rate, 44_100.0);
        assert_eq!(audio.channels.len(), 2);
        assert_eq!(audio.frames(), 1000);
        assert!((audio.channels[0][500] - 5000.0 / 32768.0).abs() < 1e-6);
        assert!((audio.channels[1][500] + 5000.0 / 32768.0).abs() < 1e-6);
    }
}
