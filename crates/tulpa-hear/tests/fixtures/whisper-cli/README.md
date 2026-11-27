`w1-0-59.8.ru.json`: whisper-cli's own JSON (`-ojf`, full token output), unmodified, for a 59.8 s piece of test
speech (TTS, English) the ears lost live on 2026-11-24: model `ggml-large-v3-turbo-q5_0`, `-l ru`, as the live path
then held whisper. Fifteen segments of real English words, then a sixteenth, «Субтитры создавал DimaTorzok», that
whisper invented out of its training data. Read by `tests/whisper_fixture.rs`.
