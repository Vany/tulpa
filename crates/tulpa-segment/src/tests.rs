#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::suboptimal_flops,
    reason = "test signal generators turn seconds and floats into samples"
)]

use super::*;

const SR: f32 = 16_000.0;

/// `secs` of silence with a little noise at `level` RMS-ish.
fn quiet(secs: f32, level: f32) -> Vec<i16> {
    let n = (secs * SR) as usize;
    // A deterministic pseudo-noise: an LCG, enough for a floor.
    let mut seed: u32 = 12_345;
    (0..n)
        .map(|_| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let r = ((seed >> 16) & 0x7fff) as f32 / 32_768.0 - 0.5;
            (r * level * 3.4) as i16
        })
        .collect()
}

/// A voiced sound: a pitch gliding from `f0` to `f1` Hz with harmonics, at amplitude `amp`.
fn voice(secs: f32, f0: f32, f1: f32, amp: f32) -> Vec<i16> {
    let n = (secs * SR) as usize;
    let mut phase = 0.0f32;
    (0..n)
        .map(|i| {
            let t = i as f32 / n as f32;
            let f = f0 + (f1 - f0) * t;
            phase += std::f32::consts::TAU * f / SR;
            let v = phase.sin() + 0.5 * (2.0 * phase).sin() + 0.25 * (3.0 * phase).sin();
            (v * amp / 1.75) as i16
        })
        .collect()
}

/// Speech-like: `voice` under a syllable envelope (4 Hz, between 0.1 and 1.0). The dips run near the
/// room's quiet — real speech leaves the gate's floor room to breathe between syllables; a shallower
/// envelope is a music bed by this gate's design, and it eats it (see
/// `a_rising_noise_floor_follows_the_room`).
fn speech(secs: f32, f0: f32, amp: f32) -> Vec<i16> {
    voice(secs, f0, f0, amp)
        .into_iter()
        .enumerate()
        .map(|(i, v)| {
            let t = i as f32 / SR;
            let env = 0.55 + 0.45 * (std::f32::consts::TAU * 4.0 * t).sin();
            (f32::from(v) * env) as i16
        })
        .collect()
}

fn run(cfg: Config, audio: &[i16]) -> Vec<Utterance> {
    let mut s = Segmenter::new(cfg);
    let mut out = Vec::new();
    for chunk in audio.chunks(1000) {
        out.extend(s.push(chunk));
    }
    out.extend(s.finish());
    out
}

fn cat(parts: &[Vec<i16>]) -> Vec<i16> {
    parts.iter().flatten().copied().collect()
}

/// The cutter's mechanics at short values (0.7 s close, 20 s max), quicker to feed than the ears' defaults.
fn short() -> Config {
    Config {
        close_silence: 0.7,
        max_len: 20.0,
        ..Config::default()
    }
}

/// vany 2026-11-18: the ears' defaults — a 2 s pause ends the phrase (his end-of-turn pauses are
/// at least that), a minute is the most; a mid-thought gap under it keeps it whole.
#[test]
fn the_defaults_keep_a_thought_whole() {
    let c = Config::default();
    assert!(
        (c.close_silence - 2.0).abs() < f32::EPSILON && (c.max_len - 60.0).abs() < f32::EPSILON
    );
    let audio = cat(&[
        quiet(1.0, 60.0),
        voice(1.0, 140.0, 140.0, 6000.0),
        quiet(1.5, 60.0),
        voice(1.0, 140.0, 140.0, 6000.0),
        quiet(3.0, 60.0),
        voice(1.0, 150.0, 150.0, 6000.0),
        quiet(3.0, 60.0),
    ]);
    let u = run(c, &audio);
    assert_eq!(
        u.len(),
        2,
        "a 1.5 s gap keeps it whole, 3 s ends it: {:?}",
        u.iter().map(Utterance::ms).collect::<Vec<_>>()
    );
}

#[test]
fn a_phrase_with_short_gaps_is_one_utterance_and_a_long_pause_splits() {
    // «Займёмся… ригидом»: two words 300 ms apart stay one utterance; a 1.2 s pause starts another.
    let audio = cat(&[
        quiet(1.0, 60.0),
        voice(0.8, 140.0, 140.0, 6000.0),
        quiet(0.3, 60.0),
        voice(0.7, 140.0, 140.0, 6000.0),
        quiet(1.2, 60.0),
        voice(1.0, 150.0, 150.0, 6000.0),
        quiet(1.0, 60.0),
    ]);
    let u = run(short(), &audio);
    assert_eq!(
        u.len(),
        2,
        "lengths: {:?}",
        u.iter().map(Utterance::ms).collect::<Vec<_>>()
    );
    let first = u.first().map_or(0, Utterance::ms);
    // 0.8 + 0.3 + 0.7 of sound, plus pre-roll and post-roll.
    assert!((1_800..=2_300).contains(&first), "first {first} ms");
    assert!(u.iter().all(|x| !x.cut));
}

#[test]
fn too_little_speech_is_dropped() {
    let audio = cat(&[
        quiet(1.0, 60.0),
        voice(0.2, 150.0, 150.0, 6000.0),
        quiet(1.5, 60.0),
    ]);
    assert_eq!(run(Config::default(), &audio), []);
}

#[test]
fn rising_and_falling_ends_are_told_apart() {
    // The lead sits out the warmup second: what plays inside it names the floor (see
    // `a_restart_into_speech_waits_for_the_pause`), and these tones are not the room.
    let rising = cat(&[
        quiet(1.2, 60.0),
        voice(1.2, 140.0, 140.0, 6000.0),
        voice(0.6, 140.0, 220.0, 6000.0),
        quiet(1.0, 60.0),
    ]);
    let falling = cat(&[
        quiet(1.2, 60.0),
        voice(1.2, 220.0, 220.0, 6000.0),
        voice(0.6, 220.0, 140.0, 6000.0),
        quiet(1.0, 60.0),
    ]);
    let flat = cat(&[
        quiet(1.2, 60.0),
        voice(1.8, 160.0, 162.0, 6000.0),
        quiet(1.0, 60.0),
    ]);
    let one = |a: &[i16]| {
        run(Config::default(), a)
            .into_iter()
            .next()
            .map(|u| u.intonation)
    };
    let r = one(&rising).map(|i| i.contour);
    let f = one(&falling).map(|i| i.contour);
    let fl = one(&flat);
    assert_eq!(r, Some(Contour::Rising));
    assert_eq!(f, Some(Contour::Falling));
    assert_eq!(fl.map(|i| i.contour), Some(Contour::Flat));
    // The pitch itself is near the tone's.
    let hz = fl.map_or(0.0, |i| i.f0_end);
    assert!((150.0..175.0).contains(&hz), "f0 {hz}");
}

#[test]
fn continuous_speech_is_cut_at_the_max_and_goes_on() {
    // 30 s of speech with no pause with a slightly quieter dip at 19 s: the cut lands in the last 2 s before 20 s.
    // The lead sits out the warmup second (speech inside it would name the floor).
    let audio = cat(&[
        quiet(1.2, 60.0),
        speech(19.0, 150.0, 6000.0),
        voice(0.2, 150.0, 150.0, 3000.0),
        speech(11.0, 150.0, 6000.0),
        quiet(1.5, 60.0),
    ]);
    let u = run(short(), &audio);
    assert!(u.len() >= 2, "{} utterances", u.len());
    let first = u.first().map_or(0, Utterance::ms);
    assert!((17_500..=20_100).contains(&first), "first {first} ms");
    assert!(
        u.first().is_some_and(|x| x.cut),
        "the first was cut at the max"
    );
    assert!(
        u.last().is_some_and(|x| !x.cut),
        "the last ended at a pause"
    );
    let total: u64 = u.iter().map(Utterance::ms).sum();
    assert!(
        total >= 29_000,
        "nothing of the speech lost: {total} ms in {:?}",
        u.iter().map(Utterance::ms).collect::<Vec<_>>()
    );
}

#[test]
fn a_rising_noise_floor_follows_the_room() {
    // Steady background (music-like) far above silence: after it settles it is the floor, not speech;
    // speech louder than it still comes through.
    let bed = voice(30.0, 330.0, 330.0, 1500.0);
    let mut audio = quiet(0.5, 60.0);
    audio.extend(bed.iter().take(16_000 * 25));
    let speech = voice(1.0, 140.0, 140.0, 9000.0);
    let tail: Vec<i16> = bed
        .iter()
        .skip(16_000 * 25)
        .take(16_000 * 3)
        .copied()
        .collect();
    let mixed: Vec<i16> = speech
        .iter()
        .zip(tail.iter())
        .map(|(a, b)| a.saturating_add(*b))
        .collect();
    audio.extend(mixed);
    audio.extend(bed.iter().skip(16_000 * 26).take(16_000 * 3));
    let mut s = Segmenter::new(Config::default());
    let mut out = Vec::new();
    for c in audio.chunks(1000) {
        out.extend(s.push(c));
    }
    out.extend(s.finish());
    assert!(
        s.floor() > 400.0,
        "the floor rose to the bed: {}",
        s.floor()
    );
    // The bed alone never makes an utterance longer than the max; the loud speech is in one of them.
    assert!(out.iter().all(|u| u.ms() <= 20_200));
    assert!(!out.is_empty(), "the speech over the bed came through");
}

#[test]
fn the_wav_header_is_canonical() {
    let w = wav(&[1, -1, 2]);
    assert_eq!(w.len(), 44 + 6);
    assert_eq!(w.get(..4), Some(&b"RIFF"[..]));
    assert_eq!(w.get(40..44), Some(&6u32.to_le_bytes()[..]));
    assert_eq!(w.get(24..28), Some(&16_000u32.to_le_bytes()[..]));
}

#[test]
fn the_json_line_names_the_contour() {
    let i = Intonation {
        contour: Contour::Rising,
        f0_start: 140.0,
        f0_end: 210.5,
    };
    assert_eq!(
        i.json(1500),
        "{\"intonation\":\"rising\",\"f0_start\":140.0,\"f0_end\":210.5,\"ms\":1500}"
    );
}

/// akira: a pitch YIN couldn't find is `null`, never `NaN` (not JSON).
#[test]
fn a_missing_pitch_is_null() {
    let i = Intonation {
        contour: Contour::Unknown,
        f0_start: f32::NAN,
        f0_end: f32::INFINITY,
    };
    assert_eq!(
        i.json(800),
        "{\"intonation\":\"unknown\",\"f0_start\":null,\"f0_end\":null,\"ms\":800}"
    );
}

/// akira: each utterance knows where it sits in the stream — its span ends at its last kept frame, not when the
/// close's 2 s of quiet ran out.
#[test]
fn an_utterance_knows_its_span_in_the_stream() {
    let audio = cat(&[
        quiet(2.0, 60.0),
        voice(1.5, 140.0, 140.0, 6000.0),
        quiet(6.0, 60.0),
    ]);
    let u = run(Config::default(), &audio);
    assert_eq!(u.len(), 1);
    let Some(first) = u.first() else { return };
    let (s, e) = (first.start_ms, first.end_ms);
    // Speech 2.0–3.5 s; pre-roll 0.2 s (plus the start run), post-roll 0.15 s; frames of 30 ms.
    assert!((1_650..=2_000).contains(&s), "start {s}");
    assert!(
        (3_500..=3_750).contains(&e),
        "end {e}, not ~8.5 s where the close fired"
    );
    assert_eq!(
        e - s,
        first.ms().div_ceil(30) * 30,
        "span = length, in whole frames"
    );
}

/// `secs` of room noise at ~`level` RMS, with a 0.2 s run of exact zeros every 0.7 s — the
/// avfoundation → aresample dropout pattern of a real quiet room (≈13% zero frames).
fn noisy_with_dropouts(secs: f32, level: f32) -> Vec<i16> {
    let mut out = Vec::new();
    let mut left = (secs * SR) as usize;
    while left > 0 {
        let take = left.min(8_000); // 0.5 s of noise
        out.extend(quiet(take as f32 / SR, level));
        let z = left.saturating_sub(take).min(3_200); // then 0.2 s of exact zeros
        out.extend(std::iter::repeat_n(0, z));
        left = left.saturating_sub(take).saturating_sub(z);
    }
    out
}

/// The 2026-11-18 incident: zero-run dropouts dragged the floor to its minimum, the room's
/// noise read as speech, and the close never fired. With dropouts skipped the floor holds,
/// the noise stays quiet, and only the voice becomes an utterance. The stream opens in the
/// noisy room — the realistic cold start (the tray begins while the room is already loud),
/// where the floor seeds from the first real frame; a quiet→loud step mid-stream is the
/// slow-up EMA's separate, designed adaptation case (see `a_rising_noise_floor_follows_the_room`).
#[test]
fn zero_dropouts_leave_the_floor_and_count_as_silence() {
    let audio = cat(&[
        noisy_with_dropouts(4.0, 470.0),
        voice(1.2, 140.0, 140.0, 9000.0),
        noisy_with_dropouts(4.0, 470.0),
    ]);
    let mut s = Segmenter::new(Config::default());
    let mut out = Vec::new();
    for chunk in audio.chunks(1000) {
        out.extend(s.push(chunk));
    }
    out.extend(s.finish());
    assert_eq!(
        out.len(),
        1,
        "the noise with dropouts is not speech: {:?}",
        out.iter().map(Utterance::ms).collect::<Vec<_>>()
    );
    assert!(
        s.floor() > 300.0,
        "the zeros did not drag the floor down: {}",
        s.floor()
    );
    assert!(
        s.speech_frames() < 120,
        "10 s of noisy room is quiet: {} speech frames of {}",
        s.speech_frames(),
        s.floor()
    );
}

/// The warmup trade, by design: a capture that restarts into ongoing speech names the
/// speaker's own voice as the floor and stays deaf until their first pause pulls it down —
/// the price of never naming a fade-in (or a slam) instead. The phrase after the pause is
/// heard whole.
#[test]
fn a_restart_into_speech_waits_for_the_pause() {
    let audio = cat(&[
        voice(2.0, 150.0, 150.0, 6000.0),
        quiet(1.0, 60.0),
        voice(1.0, 150.0, 150.0, 6000.0),
        quiet(2.5, 60.0),
    ]);
    let u = run(Config::default(), &audio);
    assert_eq!(
        u.len(),
        1,
        "the phrase inside warmup is the floor's, not the gate's: {:?}",
        u.iter().map(|x| (x.start_ms, x.end_ms)).collect::<Vec<_>>()
    );
    let start = u.first().map_or(0, |x| x.start_ms);
    assert!(
        (2_500..=3_600).contains(&start),
        "the heard phrase begins after the pause: {start}"
    );
}

/// A minimal RIFF reader for one fixture: 16 kHz mono s16le PCM, `data` at 44.
fn read_wav_16k_mono(bytes: &[u8]) -> Vec<i16> {
    assert_eq!(bytes.get(..4), Some(&b"RIFF"[..]), "RIFF");
    assert_eq!(bytes.get(8..12), Some(&b"WAVE"[..]), "WAVE");
    assert_eq!(bytes.get(12..16), Some(&b"fmt "[..]), "fmt");
    assert_eq!(bytes.get(20..22), Some(&1u16.to_le_bytes()[..]), "PCM");
    assert_eq!(bytes.get(22..24), Some(&1u16.to_le_bytes()[..]), "mono");
    assert_eq!(
        bytes.get(24..28),
        Some(&16_000u32.to_le_bytes()[..]),
        "16 kHz"
    );
    assert_eq!(bytes.get(34..36), Some(&16u16.to_le_bytes()[..]), "s16le");
    assert_eq!(bytes.get(36..40), Some(&b"data"[..]), "data");
    bytes
        .get(44..)
        .unwrap_or(&[])
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| i16::from_le_bytes(*p))
        .collect()
}

/// The real silent-room chunk from the 2026-11-18 incident (58.75 s, ≈13% exact-zero frames,
/// noise RMS p50 ≈461), kept outside the public repo: with dropouts skipped the gate calls
/// almost all of it quiet — the 2 s close would fire on a real Mac. Its path comes from
/// `TULPA_TEST_ROOM_CHUNK` (a rig sets it to its copy; no absolute path belongs in the repo);
/// ignored where the fixture is absent (CI), so it never passes without having run: a rig runs it with
/// `--ignored`, and there a chunk that cannot be read fails.
#[test]
#[ignore = "needs the real-room chunk: set TULPA_TEST_ROOM_CHUNK and run with --ignored"]
fn a_silent_room_chunk_never_becomes_speech() {
    let chunk = std::env::var_os("TULPA_TEST_ROOM_CHUNK")
        .unwrap_or_else(|| std::ffi::OsString::from("test-data/silent-room-chunk.wav"));
    let bytes = std::fs::read(&chunk);
    assert!(
        bytes.is_ok(),
        "no real-room chunk at {}: {bytes:?}",
        chunk.to_string_lossy()
    );
    let Ok(bytes) = bytes else { return };
    let audio = read_wav_16k_mono(&bytes);
    assert!(
        audio.len() > 16_000 * 30,
        "a real chunk, half a minute at least: {} samples",
        audio.len()
    );
    let mut s = Segmenter::new(Config::default());
    let mut out = Vec::new();
    for chunk in audio.chunks(1000) {
        out.extend(s.push(chunk));
    }
    out.extend(s.finish());
    assert!(
        out.is_empty(),
        "a silent room closes nothing (the incident: the whole chunk as speech, cut at the max): {:?}",
        out.iter().map(|u| (u.ms(), u.cut)).collect::<Vec<_>>()
    );
    assert!(
        s.speech_frames() <= 200,
        "replay target ≈128 (was 1658 before the fix): {} of {} frames",
        s.speech_frames(),
        audio.len() / FRAME
    );
    assert!(
        (150.0..800.0).contains(&s.floor()),
        "the floor settled near the room's noise (replay: ≈362): {}",
        s.floor()
    );
}

#[test]
fn the_anchor_is_where_the_samples_begin_and_keeps_the_earliest() {
    let mut a = Anchor::default();
    // A stream whose first second of audio is read 2 s after its reader started at 1_000_000.
    assert_eq!(a.read(16_000, 1_003_000), (1_002_000, None));
    // A late read (it waited 300 ms) does not move it; a backlog of 600 ms is said, then not again within 10 s.
    assert_eq!(a.read(16_000, 1_004_300), (1_002_000, None));
    assert_eq!(a.read(16_000, 1_005_600), (1_002_000, Some(600)));
    assert_eq!(a.read(16_000, 1_006_700), (1_002_000, None));
    // An earlier estimate wins.
    assert_eq!(a.read(16_000, 1_006_900), (1_001_900, None));
    assert_eq!(a.at(), 1_001_900);
}

#[test]
fn a_restarted_anchor_takes_the_gap_and_keeps_the_count() {
    let mut a = Anchor::default();
    assert_eq!(a.read(16_000, 1_001_000), (1_000_000, None));
    assert_eq!(a.read_ms(), 1_000);
    // The stream comes back 5 s later: without a restart its earliest estimate would stay.
    a.restart();
    assert_eq!(a.read(16_000, 1_007_000).0, 1_005_000);
    assert_eq!(a.read_ms(), 2_000);
}
