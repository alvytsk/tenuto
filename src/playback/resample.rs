use std::path::PathBuf;

use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{
    Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

use super::error::PlaybackError;

const CHUNK: usize = 1024;
/// Safety cap on the number of extra silent chunks `finish` will push through
/// the resampler while draining its delay line. The delay is a small multiple
/// of `sinc_len`, far smaller than one `CHUNK`, so a single flush chunk is
/// normally enough; this bound only guards against ever looping forever.
const MAX_FLUSH_CHUNKS: usize = 8;

/// Channel mapping plus optional band-limited rate conversion.
///
/// Resampler startup delay is trimmed **once per creation or reset** — not on
/// every anchor update — and the EOF flush recovers the delayed tail so that
/// startup trimming does not shorten playback.
pub struct Converter {
    source_channels: u16,
    target_channels: u16,
    ratio: f64,
    resampler: Option<Async<f32>>,
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
    /// Output frames still to be discarded from the resampler's priming delay.
    trim_remaining: usize,
    input_frames_seen: u64,
    /// Output frames handed to callers since creation or the last `reset`.
    ///
    /// Counted here rather than inferred from the caller's buffer, because a
    /// streaming caller drains that buffer between calls: its length says how
    /// much this call produced, never how much the conversion has produced.
    emitted_frames: u64,
    /// Input frames consumed but never converted — dropped when the
    /// resampler rejected a chunk — since creation or the last `reset`.
    /// `finish` reports the total so the flush cannot mask the loss.
    dropped_frames: u64,
}

impl Converter {
    pub fn new(
        source_rate: u32,
        target_rate: u32,
        source_channels: u16,
        target_channels: u16,
    ) -> Result<Self, PlaybackError> {
        let ratio = f64::from(target_rate) / f64::from(source_rate);
        let channels = usize::from(source_channels.max(1));
        let resampler = if source_rate == target_rate {
            None
        } else {
            let params = SincInterpolationParameters {
                sinc_len: 256,
                f_cutoff: None,
                oversampling_factor: 256,
                interpolation: SincInterpolationType::Linear,
                window: WindowFunction::BlackmanHarris2,
            };
            Some(
                Async::<f32>::new_sinc(ratio, 2.0, &params, CHUNK, channels, FixedAsync::Input)
                    .map_err(|source| PlaybackError::UnsupportedInput {
                        path: PathBuf::default(),
                        reason: format!(
                            "cannot resample {source_rate} Hz to {target_rate} Hz: {source}"
                        ),
                    })?,
            )
        };
        let trim_remaining = resampler.as_ref().map_or(0, Resampler::output_delay);
        let output_capacity = resampler
            .as_ref()
            .map_or(CHUNK, Resampler::output_frames_max);
        Ok(Self {
            source_channels: source_channels.max(1),
            target_channels: target_channels.max(1),
            ratio,
            resampler,
            input: vec![Vec::new(); channels],
            output: vec![vec![0.0; output_capacity]; channels],
            trim_remaining,
            input_frames_seen: 0,
            emitted_frames: 0,
            dropped_frames: 0,
        })
    }

    /// Discard resampler state at a discontinuity, and re-arm the startup trim.
    pub fn reset(&mut self) {
        if let Some(resampler) = self.resampler.as_mut() {
            resampler.reset();
            self.trim_remaining = resampler.output_delay();
        }
        for plane in &mut self.input {
            plane.clear();
        }
        self.input_frames_seen = 0;
        self.emitted_frames = 0;
        self.dropped_frames = 0;
    }

    pub fn expected_output_frames(&self, input_frames: u64) -> u64 {
        (input_frames as f64 * self.ratio).round() as u64
    }

    pub fn push(&mut self, planes: &[Vec<f32>], out: &mut Vec<f32>) {
        let frames = planes.first().map_or(0, Vec::len);
        self.input_frames_seen += frames as u64;
        if self.resampler.is_none() {
            self.interleave_planes(planes, frames, out);
            self.emitted_frames += frames as u64;
            return;
        }
        for (plane, buffered) in planes.iter().zip(self.input.iter_mut()) {
            buffered.extend_from_slice(plane);
        }
        while self.buffered_frames() >= CHUNK {
            self.process_chunk(CHUNK, None, out);
            for plane in &mut self.input {
                // The chunk is spent whether or not the resampler accepted
                // it, and a rejected one can leave planes unequal — drain
                // what each plane holds, never more.
                let consumed = CHUNK.min(plane.len());
                plane.drain(..consumed);
            }
        }
    }

    /// Feed the final partial chunk with `partial_len` so the resampler emits
    /// its delayed valid output, then drain any remaining delay with silent
    /// chunks and trim back to the expected extent.
    ///
    /// Both the flush and the trim are measured against the conversion's own
    /// running total, not against `out`. A caller that drains `out` into a ring
    /// between calls hands this an almost empty buffer, and a flush sized from
    /// that would append several chunks of pure silence and report frames the
    /// media does not contain.
    ///
    /// If resampler errors dropped input frames on the way here, the flush
    /// pads this segment back up to its expected extent with silence, so the
    /// loss is reported here rather than left invisible in the frame counts.
    pub fn finish(&mut self, out: &mut Vec<f32>) {
        if self.resampler.is_none() {
            return;
        }
        let before = self.emitted_frames;
        let expected = self.expected_output_frames(self.input_frames_seen);
        let remaining = self.buffered_frames();
        if remaining > 0 {
            for plane in &mut self.input {
                plane.resize(CHUNK, 0.0);
            }
            self.process_chunk(CHUNK, Some(remaining), out);
            for plane in &mut self.input {
                plane.clear();
            }
        }
        let mut flushes = 0;
        while self.emitted_frames < expected && flushes < MAX_FLUSH_CHUNKS {
            for plane in &mut self.input {
                plane.resize(CHUNK, 0.0);
            }
            self.process_chunk(CHUNK, Some(0), out);
            for plane in &mut self.input {
                plane.clear();
            }
            flushes += 1;
        }
        // Trim only what this call appended: everything before `before` either
        // belongs to an earlier call or has already left in someone else's ring.
        let overshoot = self.emitted_frames.saturating_sub(expected);
        let drop = overshoot.min(self.emitted_frames - before) as usize;
        if drop > 0 {
            let samples = drop * usize::from(self.target_channels);
            out.truncate(out.len().saturating_sub(samples));
            self.emitted_frames -= drop as u64;
        }
        if self.dropped_frames > 0 {
            tracing::warn!(
                dropped_input_frames = self.dropped_frames,
                emitted = self.emitted_frames,
                expected,
                "the resampler dropped input frames; that much audio is \
                 silence or missing at the end of this segment"
            );
        }
    }

    fn buffered_frames(&self) -> usize {
        self.input.first().map_or(0, Vec::len)
    }

    /// Count and log a chunk the resampler refused: its `frames` valid input
    /// frames were consumed and will never be converted, so the loss has to
    /// be visible instead of silent.
    fn record_dropped_chunk(&mut self, frames: usize, what: &str, error: &impl std::fmt::Display) {
        self.dropped_frames += frames as u64;
        tracing::warn!(frames, %error, "{what}; dropping the chunk's audio");
    }

    fn process_chunk(&mut self, len: usize, partial: Option<usize>, out: &mut Vec<f32>) {
        let channels = self.input.len();
        let output_capacity = self.output.first().map_or(0, Vec::len);
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: partial,
            active_channels_mask: None,
        };
        let Some(resampler) = self.resampler.as_mut() else {
            return;
        };
        // Valid input frames at stake in this chunk: the whole chunk mid-
        // stream, only the real tail when flushing a partial one, none when
        // the chunk is pure flush padding.
        let frames = partial.map_or(len, |valid| valid.min(len));
        let input = match SequentialSliceOfVecs::new(&self.input[..], channels, len) {
            Ok(input) => input,
            Err(error) => {
                self.record_dropped_chunk(frames, "cannot wrap the resampler input planes", &error);
                return;
            }
        };
        let mut output =
            match SequentialSliceOfVecs::new_mut(&mut self.output[..], channels, output_capacity) {
                Ok(output) => output,
                Err(error) => {
                    self.record_dropped_chunk(
                        frames,
                        "cannot wrap the resampler output planes",
                        &error,
                    );
                    return;
                }
            };
        match resampler.process_into_buffer(&input, &mut output, Some(&indexing)) {
            Ok((_, produced)) => {
                let start = self.trim_remaining.min(produced);
                self.trim_remaining -= start;
                let usable = produced - start;
                if usable == 0 {
                    return;
                }
                let planes: Vec<Vec<f32>> = self
                    .output
                    .iter()
                    .map(|plane| plane[start..start + usable].to_vec())
                    .collect();
                self.interleave_planes(&planes, usable, out);
                self.emitted_frames += usable as u64;
            }
            Err(error) => {
                self.record_dropped_chunk(frames, "the resampler rejected the chunk", &error);
            }
        }
    }

    fn interleave_planes(&self, planes: &[Vec<f32>], frames: usize, out: &mut Vec<f32>) {
        match (self.source_channels, self.target_channels) {
            (1, 2) => {
                for &sample in planes[0].iter().take(frames) {
                    out.push(sample);
                    out.push(sample);
                }
            }
            (2, 1) => {
                for (&a, &b) in planes[0].iter().zip(planes[1].iter()).take(frames) {
                    out.push((a + b) * 0.5);
                }
            }
            _ => {
                let channels = usize::from(self.target_channels).min(planes.len());
                for i in 0..frames {
                    for plane in planes.iter().take(channels) {
                        out.push(plane[i]);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use tracing_subscriber::prelude::*;

    fn sine(frames: usize, channels: usize) -> Vec<Vec<f32>> {
        (0..channels)
            .map(|_| (0..frames).map(|i| (i as f32 * 0.05).sin()).collect())
            .collect()
    }

    /// Captures every tracing event this thread emits as one
    /// `name=value`-joined line, so tests can assert on the logs the
    /// conversion produces rather than on internal state.
    struct Capture(Arc<Mutex<Vec<String>>>);

    impl<S> tracing_subscriber::Layer<S> for Capture
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = FieldText(String::new());
            event.record(&mut fields);
            self.0.lock().unwrap().push(fields.0);
        }
    }

    struct FieldText(String);

    impl tracing::field::Visit for FieldText {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            if field.name() == "message" {
                self.0.push_str(&format!("{value:?}"));
            } else {
                self.0.push_str(&format!("{}={value:?}", field.name()));
            }
        }
    }

    fn captured_logs() -> (
        tracing_subscriber::layer::Layered<Capture, tracing_subscriber::Registry>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Capture(Arc::clone(&logs)));
        (subscriber, logs)
    }

    #[test]
    fn a_matching_rate_passes_through_interleaved_without_a_resampler() {
        let mut converter = Converter::new(48_000, 48_000, 2, 2).unwrap();
        let mut out = Vec::new();
        converter.push(&sine(480, 2), &mut out);
        assert_eq!(out.len(), 960);
    }

    #[test]
    fn mono_sources_are_duplicated_into_a_stereo_device() {
        let mut converter = Converter::new(48_000, 48_000, 1, 2).unwrap();
        let mut out = Vec::new();
        converter.push(&sine(100, 1), &mut out);
        assert_eq!(out.len(), 200);
        for frame in out.as_chunks::<2>().0 {
            assert_eq!(frame[0], frame[1]);
        }
    }

    #[test]
    fn stereo_sources_are_averaged_into_a_mono_device() {
        let mut converter = Converter::new(48_000, 48_000, 2, 1).unwrap();
        let mut out = Vec::new();
        converter.push(&[vec![1.0; 10], vec![-1.0; 10]], &mut out);
        assert_eq!(out.len(), 10);
        assert!(out.iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn startup_delay_is_trimmed_exactly_once_per_reset() {
        // Trimming on every logical anchor update would repeatedly swallow audio.
        let mut converter = Converter::new(44_100, 48_000, 2, 2).unwrap();
        let mut first = Vec::new();
        for _ in 0..20 {
            converter.push(&sine(1024, 2), &mut first);
        }
        let mut second = Vec::new();
        for _ in 0..20 {
            converter.push(&sine(1024, 2), &mut second);
        }
        // The second run trims nothing, so it yields at least as much output.
        assert!(second.len() >= first.len());
    }

    #[test]
    fn eof_flush_recovers_the_delayed_tail_without_synthetic_padding() {
        // Startup trimming must not shorten playback: the flush returns the
        // delayed valid output, and padding is trimmed to the expected extent.
        let mut converter = Converter::new(44_100, 48_000, 2, 2).unwrap();
        let mut out = Vec::new();
        let input_frames = 44_100u64;
        for _ in 0..43 {
            converter.push(&sine(1024, 2), &mut out);
        }
        converter.push(&sine(44_100 - 43 * 1024, 2), &mut out);
        let before_flush = out.len();
        converter.finish(&mut out);
        assert!(
            out.len() > before_flush,
            "the flush must recover delayed output"
        );
        let expected = converter.expected_output_frames(input_frames) as usize * 2;
        let delta = out.len().abs_diff(expected);
        assert!(
            delta <= 2 * 2,
            "got {} samples, expected about {expected}",
            out.len()
        );
    }

    #[test]
    fn a_streaming_caller_gets_the_same_total_as_one_that_accumulates() {
        // The engine drains its buffer into the PCM ring between calls, so
        // `finish` sees an almost empty buffer. Sizing the flush from that
        // buffer appended chunks of pure silence and pushed the reported
        // position past the end of the media.
        let mut converter = Converter::new(44_100, 48_000, 2, 2).unwrap();
        let chunks = 22u64;
        let mut total = 0usize;
        let mut last = Vec::new();
        for _ in 0..chunks {
            let mut out = Vec::new();
            converter.push(&sine(1024, 2), &mut out);
            total += out.len();
            if !out.is_empty() {
                last = out;
            }
        }
        let mut tail = Vec::new();
        converter.finish(&mut tail);
        total += tail.len();
        if !tail.is_empty() {
            last = tail;
        }
        let expected = converter.expected_output_frames(chunks * 1024) as usize * 2;
        assert_eq!(
            total, expected,
            "streaming must emit exactly what the conversion predicts"
        );
        assert!(
            last.iter().rev().take(16).any(|sample| *sample != 0.0),
            "the last frames emitted must be audio, not flush padding"
        );
    }

    #[test]
    fn reset_discards_resampler_state_for_a_seek() {
        let mut converter = Converter::new(44_100, 48_000, 2, 2).unwrap();
        let mut out = Vec::new();
        converter.push(&sine(1024, 2), &mut out);
        converter.reset();
        let mut after = Vec::new();
        converter.push(&sine(1024, 2), &mut after);
        assert!(!after.is_empty());
    }

    #[test]
    fn a_rejected_chunk_is_logged_and_finish_reports_the_loss() {
        // A decoder glitch can hand `push` planes of unequal length; the
        // resampler's input adapter then rejects the chunk. The drop must be
        // observable — a warn per dropped chunk, and a finish that reports
        // the accumulated loss instead of letting the flush pad the
        // shortfall with silence.
        let (subscriber, logs) = captured_logs();
        tracing::subscriber::with_default(subscriber, || {
            let mut converter = Converter::new(44_100, 48_000, 2, 2).unwrap();
            let left: Vec<f32> = (0..2000).map(|i| (i as f32 * 0.05).sin()).collect();
            let right: Vec<f32> = left[..500].to_vec();
            let mut out = Vec::new();
            converter.push(&[left, right], &mut out);
            converter.finish(&mut out);
        });
        let lines = logs.lock().unwrap().join("\n");
        assert!(
            lines.contains("dropping the chunk's audio"),
            "each dropped chunk must warn; captured: {lines}"
        );
        assert!(
            lines.contains("frames=1024"),
            "the warn must carry the dropped input frame count; captured: {lines}"
        );
        assert!(
            lines.contains("dropped_input_frames=1024"),
            "finish must report the accumulated loss; captured: {lines}"
        );
    }

    #[test]
    fn a_clean_conversion_emits_no_resampler_warnings() {
        let (subscriber, logs) = captured_logs();
        tracing::subscriber::with_default(subscriber, || {
            let mut converter = Converter::new(44_100, 48_000, 2, 2).unwrap();
            let mut out = Vec::new();
            for _ in 0..3 {
                converter.push(&sine(1024, 2), &mut out);
            }
            converter.finish(&mut out);
        });
        let lines = logs.lock().unwrap();
        assert!(
            lines.is_empty(),
            "a clean conversion must stay silent; captured: {lines:?}"
        );
    }
}
