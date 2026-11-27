//! The real [`Recognizer`]: whisper.cpp in process through `whisper-rs` (Metal on macOS), on the speech that
//! whisper.cpp's Silero VAD finds. Only with the `whisper` feature.
//!
//! The model and the VAD are loaded once, at [`WhisperRecognizer::load`]; a missing model or VAD file is refused
//! there, by name, never skipped. Each [`transcribe`](Recognizer::transcribe) runs the VAD over the utterance's
//! samples in memory (`whisper-cli`'s default VAD parameters), joins the speech spans as whisper.cpp's own VAD
//! does ([`span::join`]), and runs whisper once over the joined speech: beam search 5 (as `whisper-cli`), token
//! timestamps on, the given language, the `EARS_PROMPT` as the initial prompt when set, nothing printed. Special
//! and timestamp tokens are dropped, and token times are mapped back through the join's pieces
//! ([`span::map_tokens`]), so they count from the utterance's first sample.
//!
//! The VAD runs here, not inside whisper: `WhisperState::full` (whisper.cpp's `whisper_full_with_state`) ignores
//! `params.vad`, and even whisper.cpp's own VAD maps only segment times back to the original audio, never token
//! times.

use std::path::Path;

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
    WhisperVadContext, WhisperVadContextParams, WhisperVadParams,
};

pub use crate::check::{MODEL_FILE, VAD_FILE, models_dir};
use crate::recognize::{RecognizeError, Recognizer, Token, Transcript};
use crate::span::{self, Joined};

/// The VAD's `samples_overlap` (s): what each joined span but the last is extended by. whisper-rs 0.16's
/// `WhisperVadParams` cannot be read back, so this one value is set into the params and used by the join alike. It
/// is the default, unchanged (whisper.cpp's `whisper_vad_default_params`, `samples_overlap = 0.1`).
const VAD_OVERLAP_S: f32 = 0.1;

/// whisper.cpp's default thread count (`whisper_full_default_params`: at most 4), for the VAD as for the model.
fn n_threads() -> i32 {
    std::thread::available_parallelism()
        .map_or(4, |n| i32::try_from(n.get()).unwrap_or(4))
        .min(4)
}

/// whisper.cpp, loaded.
pub struct WhisperRecognizer {
    ctx: WhisperContext,
    state: WhisperState,
    vad: WhisperVadContext,
    use_vad: bool,
    prompt: Option<String>,
}

impl std::fmt::Debug for WhisperRecognizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhisperRecognizer").finish_non_exhaustive()
    }
}

/// A path whisper-rs can take (UTF-8), or a load error naming it.
fn utf8(path: &Path) -> Result<String, RecognizeError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| RecognizeError::Load(format!("not a UTF-8 path: {}", path.display())))
}

impl WhisperRecognizer {
    /// Loads the model from `dir` ([`MODEL_FILE`], [`VAD_FILE`]); `prompt` is whisper's initial prompt.
    ///
    /// # Errors
    ///
    /// [`RecognizeError::MissingModel`] naming the first missing file; [`RecognizeError::Load`] when whisper.cpp
    /// cannot load the model or a path is not UTF-8 or holds a NUL.
    pub fn load(dir: &Path, prompt: Option<String>) -> Result<Self, RecognizeError> {
        let model = dir.join(MODEL_FILE);
        let vad = dir.join(VAD_FILE);
        if !model.is_file() {
            return Err(RecognizeError::MissingModel(format!(
                "model {} (whisper.cpp's ggml-large-v3-turbo q5_0)",
                model.display()
            )));
        }
        if !vad.is_file() {
            return Err(RecognizeError::MissingModel(format!(
                "VAD model {} (whisper.cpp's ggml-silero v5.1.2)",
                vad.display()
            )));
        }
        let vad = utf8(&vad)?;
        // whisper-rs panics on a NUL in these; refuse first (an env var cannot hold one, a path cannot either).
        if vad.contains('\0') || prompt.as_deref().is_some_and(|p| p.contains('\0')) {
            return Err(RecognizeError::Load(
                "a NUL in the VAD path or the prompt".to_owned(),
            ));
        }
        // whisper.cpp's own chatter (system info, timings) goes nowhere, as the old script's `2>/dev/null`.
        whisper_rs::install_logging_hooks();
        let ctx = WhisperContext::new_with_params(&model, WhisperContextParameters::default())
            .map_err(|e| RecognizeError::Load(e.to_string()))?;
        let state = ctx
            .create_state()
            .map_err(|e| RecognizeError::Load(e.to_string()))?;
        let mut vad_params = WhisperVadContextParams::default();
        vad_params.set_n_threads(n_threads());
        let vad = WhisperVadContext::new(&vad, vad_params)
            .map_err(|e| RecognizeError::Load(format!("VAD model {vad}: {e}")))?;
        Ok(Self {
            ctx,
            state,
            vad,
            use_vad: true,
            prompt: prompt.filter(|p| !p.is_empty()),
        })
    }

    /// Turns the Silero VAD on (the default, as [`load`](Self::load) leaves it) or off. Off is one whisper pass
    /// over the whole, unfiltered audio, for comparing timings (the `transcribe_wav` example's `--no-vad`); the live
    /// path never turns it off.
    pub const fn set_vad(&mut self, on: bool) {
        self.use_vad = on;
    }

    /// The utterance's speech, joined as whisper.cpp joins it (no pieces: no speech).
    fn speech(&mut self, samples_16k: &[f32]) -> Result<Joined, RecognizeError> {
        let mut params = WhisperVadParams::default();
        params.set_samples_overlap(VAD_OVERLAP_S);
        let spans: Vec<(f32, f32)> = self
            .vad
            .segments_from_samples(params, samples_16k)
            .map_err(|e| RecognizeError::Run(format!("VAD: {e}")))?
            .map(|s| (s.start, s.end))
            .collect();
        Ok(span::join(samples_16k, &spans, VAD_OVERLAP_S))
    }

    /// One whisper pass over `samples_16k`: its text tokens (times from its first sample), whether whisper gave
    /// any segment at all, and how many special and timestamp tokens its segments held.
    fn pass(
        &mut self,
        samples_16k: &[f32],
        lang: &str,
    ) -> Result<(Vec<Token>, bool, usize), RecognizeError> {
        let mut params = FullParams::new(SamplingStrategy::BeamSearch {
            beam_size: 5,
            patience: -1.0,
        });
        params.set_language(Some(lang));
        params.set_token_timestamps(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        // The VAD, when on, already ran over the whole utterance; never twice.
        params.enable_vad(false);
        if let Some(p) = &self.prompt {
            params.set_initial_prompt(p);
        }
        self.state
            .full(params, samples_16k)
            .map_err(|e| RecognizeError::Run(e.to_string()))?;
        let eot = self.ctx.token_eot();
        let mut tokens = Vec::new();
        let mut any_segment = false;
        let mut specials = 0_usize;
        for (n, seg) in self.state.as_iter().enumerate() {
            any_segment = true;
            for i in 0..seg.n_tokens() {
                let Some(tok) = seg.get_token(i) else {
                    continue;
                };
                let data = tok.token_data();
                if data.id >= eot {
                    specials = specials.saturating_add(1); // special and timestamp tokens
                    continue;
                }
                // Raw bytes: a letter may be split across tokens; the assembler decodes joined words.
                let bytes = tok
                    .to_bytes()
                    .map_err(|e| RecognizeError::Run(e.to_string()))?
                    .to_vec();
                // whisper's token times are in 10 ms units.
                let ms = |t: i64| u64::try_from(t).unwrap_or(0).saturating_mul(10);
                tokens.push(Token::new(bytes, ms(data.t0), ms(data.t1), data.p).in_segment(n));
            }
        }
        Ok((tokens, any_segment, specials))
    }
}

impl Recognizer for WhisperRecognizer {
    fn transcribe(
        &mut self,
        samples_16k: &[f32],
        lang: &str,
    ) -> Result<Transcript, RecognizeError> {
        if samples_16k.is_empty() {
            return Ok(Transcript::new(Vec::new(), true));
        }
        if !self.use_vad {
            let (tokens, any_segment, specials) = self.pass(samples_16k, lang)?;
            return Ok(Transcript::new(tokens, !any_segment).with_specials(specials));
        }
        let joined = self.speech(samples_16k)?;
        if joined.pieces.is_empty() {
            // No speech: no pass, and the same no-speech transcript as whisper finding no segment, so the retry
            // treats both alike.
            return Ok(Transcript::new(Vec::new(), true));
        }
        let (tokens, any_segment, specials) = self.pass(&joined.samples, lang)?;
        Ok(
            Transcript::new(span::map_tokens(&joined.pieces, tokens), !any_segment)
                .with_specials(specials),
        )
    }
}
