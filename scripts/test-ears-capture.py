#!/usr/bin/env python3
"""Exercise the ears' capture script against stubbed capture, segmenter and whisper.

Run from the repository root. The capture script runs with a temporary HOME (its
state dir and model files), stub helpers on its PATH (ffmpeg and sysaudio that
idle; a segmenter that publishes scripted wav+json pieces exactly as the real
one does — the JSON first, both renamed into place; a whisper-cli that answers
--version with a scripted line and hands back scripted -ojf transcripts in
whisper.cpp 1.9.5/1.9.6's TAB-indented layout) and the bundle's helper dir pointed at
the same stubs; nothing outside the temporary directory is read, written or run.
"""

import os
import pathlib
import select
import shutil
import signal
import subprocess
import tempfile
import textwrap
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
EARS = ROOT / "packaging/macos/ears-capture.sh"
FIXTURES = ROOT / "scripts/fixtures/0393-ojf"  # raw -ojf captures off work-mac, 2026-11-19

SEGMENT_STUB = textwrap.dedent(
    """\
    #!/bin/sh
    dir=
    while [ $# -gt 0 ]; do
      case "$1" in
        --dir) dir=$2; shift 2;;
        *) shift;;
      esac
    done
    [ -n "$dir" ] || exit 2
    mkdir -p "$dir"
    [ -e "$dir/.rig-done" ] && { sleep 600; exit 0; }
    : > "$dir/.rig-done"
    while IFS=' ' read -r name ms t0 t1 cutf into size; do
      [ -n "$name" ] || continue
      j="{\\"intonation\\":\\"$into\\",\\"f0_start\\":0,\\"f0_end\\":0,\\"ms\\":$ms,\\"t0\\":$t0,\\"t1\\":$t1"
      [ "$cutf" = cut ] && j="$j,\\"cut\\":true"
      printf '%s' "$j}" > "$dir/.$name.json.part" && mv "$dir/.$name.json.part" "$dir/$name.json"
      if [ -n "$size" ] && [ "$size" -gt 0 ] 2>/dev/null; then
        head -c "$size" /dev/zero > "$dir/.$name.wav.part"
      else
        : > "$dir/.$name.wav.part"
      fi
      mv "$dir/.$name.wav.part" "$dir/$name.wav"
    done < "$TEST_PIECES/$(basename "$dir").pieces"
    sleep 600
    """
)

WHISPER_STUB = textwrap.dedent(
    """\
    #!/bin/sh
    if [ "$1" = --version ]; then
      printf '%s\\n' "$TEST_WHISPER_VERSION"
      exit 0
    fi
    of=; f=
    while [ $# -gt 0 ]; do
      case "$1" in
        -of) of=$2; shift 2;;
        -f) f=$2; shift 2;;
        *) shift;;
      esac
    done
    [ -n "$of" ] && [ -n "$f" ] || exit 2
    n=$(basename "${f%.wav}")
    cp "$TEST_TRANSCRIPTS/$n.json" "$of.json"
    [ -e "$TEST_TRANSCRIPTS/$n.unreadable" ] && chmod 000 "$of.json"   # the assembler cannot read it
    exit 0
    """
)


def ojf(tokens):
    """whisper.cpp 1.9.5/1.9.6's -ojf JSON as the assembler parses it: the tokens'
    array at three TABs, each token's text at five, its offsets' numbers at six."""
    lines = ["{", '\t"transcription": [', "\t\t{", '\t\t\t"tokens": [']
    last = len(tokens) - 1
    for i, (text, start, end) in enumerate(tokens):
        lines += [
            "\t\t\t\t{",
            f'\t\t\t\t\t"text": "{text}",',
            '\t\t\t\t\t"offsets": {',
            f'\t\t\t\t\t\t"from": {start},',
            f'\t\t\t\t\t\t"to": {end}',
            "\t\t\t\t\t}",
            "\t\t\t\t}," if i != last else "\t\t\t\t}",
        ]
    lines += ["\t\t\t]", "\t\t}", "\t]", "}"]
    return "\n".join(lines) + "\n"


DEBUG = "TULPA-EARS-DEBUG "


class Rig:
    def __init__(self, script=EARS):
        self.script = script
        self.debug = []  # the TULPA-EARS-DEBUG records seen by `lines`, in order
        self.home = pathlib.Path(tempfile.mkdtemp(prefix="tulpa-ears-rig-"))
        self.bin = self.home / "bin"
        self.bin.mkdir()
        for name, text in (
            ("ffmpeg", "#!/bin/sh\nsleep 600\n"),
            ("tulpa-sysaudio", "#!/bin/sh\nsleep 600\n"),
            ("tulpa-segment", SEGMENT_STUB),
            ("whisper-cli", WHISPER_STUB),
        ):
            stub = self.bin / name
            stub.write_text(text)
            stub.chmod(0o755)
        models = self.home / "models"
        models.mkdir()
        (models / "ggml-large-v3-turbo-q5_0.bin").write_bytes(b"")
        (models / "ggml-silero-v5.1.2.bin").write_bytes(b"")
        self.pieces = self.home / "pieces"
        self.pieces.mkdir()
        self.transcripts = self.home / "transcripts"
        self.transcripts.mkdir()
        self.proc = None

    def piece(self, side, name, ms, t0, t1, cut=False, into="flat", size=0):
        """One scripted utterance on one side: its segment json line, and a
        stub wav (whisper answers from `transcript`, keyed by the same name).
        `size` pads the wav with that many zero bytes — the ring's byte bound
        needs pieces with real weights."""
        cutf = "cut" if cut else "-"
        with (self.pieces / f"{side}.pieces").open("a") as f:
            f.write(f"{name} {ms} {t0} {t1} {cutf} {into} {size}\n")

    def transcript(self, name, tokens):
        (self.transcripts / f"{name}.json").write_text(ojf(tokens))

    @property
    def state(self):
        return self.home / "state"

    @property
    def ring(self):
        return self.state / "ring"

    def unreadable(self, name):
        """whisper's transcript for this piece lands unreadable (mode 000): the assembler fails on it."""
        (self.transcripts / f"{name}.unreadable").write_text("")

    def transcript_file(self, name, src):
        """A real -ojf capture becomes the transcript for one piece, verbatim."""
        shutil.copy(src, self.transcripts / f"{name}.json")

    def start(self, unset=(), **extra):
        env = os.environ.copy()
        for name in unset:
            env.pop(name, None)
        env.update(
            HOME=str(self.home),
            EARS_PATH=f"{self.bin}:/usr/bin:/bin",
            EARS_STATE=str(self.home / "state"),
            EARS_RING_DIR=str(self.home / "state" / "ring"),
            EARS_MODELS=str(self.home / "models"),
            EARS_MIC=":rig mic",
            TULPA_BUNDLE_BIN=str(self.bin),
            TULPA_LINES_STDOUT="1",
            TEST_PIECES=str(self.pieces),
            TEST_TRANSCRIPTS=str(self.transcripts),
            TEST_WHISPER_VERSION="whisper.cpp version: 1.9.5",  # the real line's shape
        )
        env.update(extra)
        self.proc = subprocess.Popen(
            ["/bin/sh", str(self.script)],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        return self.proc

    def lines(self, count, timeout=15.0):
        """Reads `count` stdout lines or fails the deadline; the capture's own
        startup line is the first, each TULPA-EARS-LINE marker one more. The
        TULPA-EARS-DEBUG records (an unread piece's verdict) do not count: they
        go to `self.debug`, in order."""
        fd = self.proc.stdout.fileno()
        out = b""
        deadline = time.monotonic() + timeout
        def counted():
            return sum(1 for l in out.split(b"\n")[:-1] if not l.startswith(DEBUG.encode()))

        while counted() < count:
            left = deadline - time.monotonic()
            if left <= 0:
                raise AssertionError(
                    f"wanted {count} lines in {timeout}s, got {counted()}:"
                    f" {out.decode('utf-8', 'replace')!r}"
                )
            ready, _, _ = select.select([fd], [], [], left)
            if not ready:
                continue
            chunk = os.read(fd, 4096)
            if not chunk:
                raise AssertionError(f"capture exited early: {out!r}")
            out += chunk
        lines = out.decode("utf-8").splitlines()
        self.debug += [l for l in lines if l.startswith(DEBUG)]
        return [l for l in lines if not l.startswith(DEBUG)]

    def refuse(self, timeout=15.0):
        out, err = self.proc.communicate(timeout=timeout)
        return self.proc.returncode, (out + err).decode("utf-8", "replace")

    def stop(self):
        if self.proc is None:
            return
        if self.proc.poll() is None:
            os.killpg(os.getpgid(self.proc.pid), signal.SIGTERM)
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(os.getpgid(self.proc.pid), signal.SIGKILL)
                self.proc.wait(timeout=5)
        for stream in (self.proc.stdout, self.proc.stderr):
            if stream:
                stream.close()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.stop()
        shutil.rmtree(self.home, ignore_errors=True)


def check_version_mismatch_refuses():
    """Only a version whose -ojf layout the assembler was verified against runs; the
    near-misses a substring match would let through, and any other shape of line, refuse
    too, naming what they said and the accepted list."""
    near = ("1.9.4", "1.9.7", "1.9.50", "11.9.5", "1.9.6-rc1", "1.9.61", "1.9.5.1")
    shapes = ("whisper-cli: version 1.9.5", "anything 1.9.6", "1.9.6", "whisper.cpp version: 1.9.5 1.9.6")
    for line in [f"whisper.cpp version: {seen}" for seen in near] + list(shapes):
        with Rig() as rig:
            rig.start(TEST_WHISPER_VERSION=line)
            rc, said = rig.refuse()
            assert rc == 1, (line, rc, said)
            assert "refusing to run" in said, said
            assert "whisper-cli 1.9.5 1.9.6 " in said, f"the accepted list: {said}"
            assert f"this one says: {line})" in said, f"the refusal names what the binary said: {said}"


def check_verified_versions_run_a_real_capture():
    """Each verified version runs; under 1.9.6, a real 1.9.6 -ojf capture (whisper.cpp's
    public jfk sample) publishes its words with their timings."""
    for seen in ("1.9.5", "1.9.6"):
        with Rig() as rig:
            rig.transcript_file("c000001", FIXTURES / "jfk.whisper-1.9.6.json")
            rig.piece("them", "c000001", 11000, 1_000_000, 1_011_000)
            rig.start(TEST_WHISPER_VERSION=f"whisper.cpp version: {seen}")
            line = rig.lines(2)[1]
            assert line.startswith("TULPA-EARS-LINE 1000000 1011000 "), (seen, line)
            text, _, payload = line.partition("\t")
            assert text.endswith(
                " [them] And so, my fellow Americans, ask not what your country can do for"
                " you, ask what you can do for your country."
            ), (seen, text)
            words = payload.split(";")
            assert len(words) == 22, (seen, payload)
            assert words[0] == "1000040,1000240,And", (seen, payload)
            assert words[-1] == "1007550,1008010,country.", (seen, payload)


def check_version_silence_refuses():
    with Rig() as rig:
        rig.start(TEST_WHISPER_VERSION="")
        rc, said = rig.refuse()
        assert rc == 1, (rc, said)
        assert "refusing to run" in said, said
        assert "nothing" in said, f"a binary that says nothing is refused by name: {said}"


def check_held_turn_keeps_its_midhold_voice():
    """Three pieces of one turn: read, unread (whisper blank), read. The unread
    middle is <voice N.Ns> inside the merged text — 'the gate heard something
    here' is part of the stream (akira 2b, 2026-11-18) — and the word timings
    of the readable pieces ride the line's TAB payload."""
    with Rig() as rig:
        rig.transcript("c000001", [("привет", 0, 500), (" как", 700, 1100)])
        rig.transcript("c000002", [])
        rig.transcript("c000003", [(" мир", 200, 900)])
        rig.piece("me", "c000001", 6000, 1_000_000, 1_006_000, cut=True)
        rig.piece("me", "c000002", 4000, 1_012_400, 1_016_400, cut=True)
        rig.piece("me", "c000003", 5000, 1_017_000, 1_022_000)
        rig.start()
        _, marker = rig.lines(2)
        text, _, payload = marker.partition("\t")
        assert text.startswith("TULPA-EARS-LINE 1000000 1022000 "), text
        body = text.split("[me] ", 1)[1]
        assert body == "привет как<pause 6.4s><voice 4.0s><pause 0.6s>мир", body
        assert payload == (
            "1000000,1000500,привет;1000700,1001100,как;"
            "1017200,1017900,мир"
        ), payload


def check_hold_bound_publishes_midphrase():
    """EARS_HOLD_MAX_MS of held speech ships the turn without a pause; the
    speech after the bound is the next line (akira 2a, 2026-11-18)."""
    with Rig() as rig:
        rig.transcript("c000001", [("один", 0, 400)])
        rig.transcript("c000002", [(" два", 600, 900)])
        rig.transcript("c000003", [(" три", 0, 300)])
        rig.piece("me", "c000001", 6000, 2_000_000, 2_006_000, cut=True)
        rig.piece("me", "c000002", 6000, 2_006_000, 2_012_000, cut=True)
        rig.piece("me", "c000003", 3000, 2_012_000, 2_015_000)
        rig.start(EARS_HOLD_MAX_MS="10000")
        _, first, second = rig.lines(3)
        assert first.startswith("TULPA-EARS-LINE 2000000 2012000 "), first
        assert "[me] один два\t" in first, first
        assert second.startswith("TULPA-EARS-LINE 2012000 2015000 "), second
        assert "[me] три\t" in second, second


def check_hold_far_from_the_bound_merges():
    """The same three pieces with the bound far away merge into one line at the
    pause: the bound publishes early, it never splits a pausing turn."""
    with Rig() as rig:
        rig.transcript("c000001", [("один", 0, 400)])
        rig.transcript("c000002", [(" два", 600, 900)])
        rig.transcript("c000003", [(" три", 0, 300)])
        rig.piece("me", "c000001", 6000, 2_000_000, 2_006_000, cut=True)
        rig.piece("me", "c000002", 6000, 2_006_000, 2_012_000, cut=True)
        rig.piece("me", "c000003", 3000, 2_012_000, 2_015_000)
        rig.start(EARS_HOLD_MAX_MS="3600000")
        _, marker = rig.lines(2)
        assert marker.startswith("TULPA-EARS-LINE 2000000 2015000 "), marker
        assert "[me] один два три\t" in marker, marker


def check_assembler_drops_whisper_markers_and_merges_pieces():
    """The first live 34d7441 line (glados, 23:19Z): whisper's own markers leaked into the text
    and the words ([_BEG_], [_TT_39] — never speech), and the turbo model's subword pieces came
    through as separate words (" П", "ров", "ер", "ка"). The assembler drops the markers whole
    and merges pieces into words — a piece with a leading space begins one, the word's span is
    its first piece's t0 to its last piece's t1, punctuation stays attached."""
    with Rig() as rig:
        rig.transcript(
            "c000001",
            [
                ("[_BEG_]", 0, 10),
                (" П", 40, 90),
                ("ров", 90, 140),
                ("ер", 140, 190),
                ("ка", 190, 240),
                (" уш", 250, 300),
                ("ей", 300, 350),
                (".", 350, 360),
                ("[_TT_39]", 360, 370),
                (" Раз", 1400, 1500),
                (",", 1500, 1510),
                (" два", 1550, 1650),
            ],
        )
        rig.piece("me", "c000001", 6000, 3_000_000, 3_006_000)
        rig.start()
        _, marker = rig.lines(2)
        text, _, payload = marker.partition("\t")
        body = text.split("[me] ", 1)[1]
        assert body == "Проверка ушей.<pause 1.0s> Раз, два", body
        assert payload == (
            "3000040,3000240,Проверка;3000250,3000360,ушей.;"
            "3001400,3001510,Раз,;3001550,3001650,два"
        ), payload


def check_a_phrase_of_only_markers_is_voice():
    """Tokens that are all whisper markers are no speech at all: the piece reads as heard-but-
    unread, <voice N.Ns>, with no words payload."""
    with Rig() as rig:
        rig.transcript("c000001", [("[_BEG_]", 0, 10), ("[_TT_5]", 20, 30), ("[_EOT_]", 40, 50)])
        rig.piece("me", "c000001", 4000, 3_000_000, 3_004_000)
        rig.start()
        _, marker = rig.lines(2)
        fields = marker.split()
        assert fields[:3] == ["TULPA-EARS-LINE", "3000000", "3004000"], marker
        assert marker.endswith(" [me] <voice 4.0s>"), marker
        assert "\t" not in marker, "an unread phrase carries no words payload"


def check_ring_keeps_consumed_pairs_and_drops_oldest():
    """The raw ring (the owner, 2026-11-19): a consumed piece's wav AND its segment json park
    under EARS_STATE/ring — never a synced path, nothing outside the state dir — and past the
    byte bound the oldest files leave first. 4 pieces of ~500 KB against a 1 MiB bound: the
    first two wavs are dropped, the newest pair rides whole, the ring never exceeds the bound,
    and the dir is 0700 with 0600 files (raw audio, the owner's policy: LOCAL, overwritten,
    never shipped off the Mac unless he pulls it)."""
    with Rig() as rig:
        for i in range(1, 5):
            rig.transcript(f"c00000{i}", [("слово", 0, 400)])
            rig.piece("me", f"c00000{i}", 3000, 5_000_000 + i * 3000, 5_003_000 + i * 3000, size=500_000)
        rig.start(EARS_RING_MAX_MB="1")
        rig.lines(5)  # startup + four shipped phrases
        ring = rig.ring
        assert ring.is_dir(), "the ring lives under EARS_STATE"
        assert ring.stat().st_mode & 0o777 == 0o700, oct(ring.stat().st_mode & 0o777)
        names = {p.name for p in ring.iterdir()}
        assert "me-c000003.wav" in names and "me-c000003.json" in names, sorted(names)
        assert "me-c000004.wav" in names and "me-c000004.json" in names, sorted(names)
        assert "me-c000001.wav" not in names and "me-c000002.wav" not in names, sorted(names)
        assert not list((rig.state / "me").glob("c*.wav")), "consumed pieces move, not copy"
        total = sum(p.stat().st_size for p in ring.iterdir())
        assert total <= 1024 * 1024, total
        for p in ring.iterdir():
            assert p.stat().st_mode & 0o777 == 0o600, (p.name, oct(p.stat().st_mode & 0o777))


def check_ring_survives_a_restart():
    """The startup wipe clears them/ and me/ but must NOT clear the ring: it exists so the owner
    can pull what was just said after a hiccup — a restart that erased it would defeat its one
    job. Second run, same EARS_STATE: the old pair is still there and the new one lands beside it."""
    with Rig() as rig:
        rig.transcript("c000001", [("первое", 0, 400)])
        rig.piece("me", "c000001", 3000, 5_000_000, 5_003_000)
        rig.start()
        rig.lines(2)
        assert (rig.ring / "me-c000001.wav").is_file()
        assert (rig.ring / "me-c000001.json").is_file()
        rig.stop()
        (rig.pieces / "me.pieces").write_text("")  # the restarted segmenter republishes only the new piece
        rig.transcript("c000002", [("второе", 0, 400)])
        rig.piece("me", "c000002", 3000, 5_010_000, 5_013_000)
        rig.start()
        rig.lines(2)
        for name in ("me-c000001.wav", "me-c000001.json", "me-c000002.wav", "me-c000002.json"):
            assert (rig.ring / name).is_file(), name


def check_ring_holds_the_selftagged_piece_too():
    """A piece the tagger marks as our own voice leaves the transcript path at the tag seam —
    its raw audio still parks in the ring: the ring keeps what the mic heard, not what whisper
    read."""
    with Rig() as rig:
        tag = rig.bin / "tag"
        tag.write_text(
            '#!/bin/sh\ncase "$1" in *c000001.wav) echo self;; *) :;; esac\n'
        )
        tag.chmod(0o755)
        rig.transcript("c000002", [("рядом", 0, 400)])
        rig.piece("me", "c000001", 3000, 5_000_000, 5_003_000)
        rig.piece("me", "c000002", 3000, 5_010_000, 5_013_000)
        rig.start(EARS_TAG=str(tag))
        # startup, c000001's drop record (it never reaches whisper), the one shippable phrase
        lines = rig.lines(3)
        assert lines[1] == "TULPA-EARS-DROP own-voice [me] ", lines
        names = {p.name for p in rig.ring.iterdir()}
        assert {"me-c000001.wav", "me-c000001.json",
                "me-c000002.wav", "me-c000002.json"} <= names, sorted(names)


def check_dropped_phrases_are_told_to_the_tray():
    """Each phrase the script heard and will not publish is one TULPA-EARS-DROP record on stdout,
    with its reason, for the tray's flow.log: our own voice by the tagger, nothing whisper could
    read, the prompt's vocabulary echoed back. The phrase after them publishes as ever."""
    with Rig() as rig:
        tag = rig.bin / "tag"
        tag.write_text('#!/bin/sh\ncase "$1" in *c000001.wav) echo self;; *) :;; esac\n')
        tag.chmod(0o755)
        # c000002 has no transcript: the whisper stub leaves no JSON.
        rig.transcript("c000003", [("Продолжение,", 0, 400), (" следует", 500, 900)])
        rig.transcript("c000004", [("настоящая", 0, 400), (" фраза", 500, 900)])
        for i in range(1, 5):
            rig.piece("me", f"c00000{i}", 3000, 1_000_000 * i, 1_000_000 * i + 3000)
        rig.start(EARS_TAG=str(tag), EARS_PROMPT="Продолжение, следует, подписывайтесь")
        lines = rig.lines(5)[1:]
        assert lines[:3] == [
            "TULPA-EARS-DROP own-voice [me] ",
            "TULPA-EARS-DROP no-transcript [me] ",
            "TULPA-EARS-DROP prompt-echo [me] Продолжение, следует",
        ], lines
        assert lines[3].startswith("TULPA-EARS-LINE 4000000 4003000 "), lines
        assert " [me] настоящая фраза" in lines[3], lines


def check_bad_ring_bound_refuses_before_any_capture():
    """F1 (akira, 2026-11-19, gap closed 16:35Z): the ring's byte bound is validated AND computed
    in preflight, BEFORE any capture side starts. A decimal or unit (`1.5`, `8MB`) used to reach
    the arithmetic after the subshells were already rolling — the shell died, the trap did not
    exist yet, and a hot mic kept writing into the state dir with no consumer. `08`/`09` pass a
    digits-only guard but are invalid octal in the arithmetic (same orphaned-mic death); `010`
    silently means 8; a huge value overflows negative and silently empties the ring — so a
    leading zero is refused outright and the value is capped. Garbage and 0 refuse too (0 is not
    "ring off": the ring is the owner's ordered policy). Every variant here proves nothing was
    spawned: no state subdirs appear and the session holds no processes 3 s later."""
    for bad in ("1.5", "8MB", "08", "010", "99999999999999"):
        with Rig() as rig:
            rig.start(EARS_RING_MAX_MB=bad)
            rc, said = rig.refuse()
            assert rc == 2, (bad, rc, said)
            assert "EARS_RING_MAX_MB" in said, (bad, said)
            assert not (rig.state / "me").exists(), (bad, "the wipe never ran")
            sid = rig.proc.pid
            time.sleep(3)
            stray = subprocess.run(
                ["ps", "-s", str(sid), "-o", "pid="], capture_output=True, text=True
            ).stdout
            assert not stray.strip(), (bad, f"stray capture processes: {stray!r}")
    for bad in ("abc", "0"):
        with Rig() as rig:
            rig.start(EARS_RING_MAX_MB=bad)
            rc, said = rig.refuse()
            assert rc == 2, (bad, rc, said)
            assert "EARS_RING_MAX_MB" in said, (bad, said)


def check_ring_is_excluded_from_time_machine():
    """F2 (akira, 2026-11-19): Time Machine excludes ~/Library/Caches by default but NOT
    ~/.cache, where the ring lives — hourly backups would ship the raw ring off the Mac, against
    the owner's policy. The script asks tmutil to exclude the ring at startup (and again whenever
    ring_keep re-creates it — the same guarded command, covered by read; macOS's own isexcluded
    answer is checked in the install slice). Off macOS tmutil is absent, so the rig stubs it and
    asserts the exclusion was asked for with the ring's path."""
    with Rig() as rig:
        log = rig.home / "tmutil.log"
        (rig.bin / "tmutil").write_text('#!/bin/sh\nprintf "%s " "$@" >> "$TMUTIL_LOG"; echo >> "$TMUTIL_LOG"\n')
        (rig.bin / "tmutil").chmod(0o755)
        rig.transcript("c000001", [("слово", 0, 400)])
        rig.piece("me", "c000001", 3000, 5_000_000, 5_003_000)
        rig.start(TMUTIL_LOG=str(log))
        rig.lines(2)
        calls = [c.strip() for c in log.read_text().splitlines()]
        assert f"addexclusion {rig.ring}" in calls, calls


def check_a_symlinked_ring_dir_refuses_and_touches_nothing():
    """F5 (akira, 2026-11-19): /tmp is writable by every local user and the first creator owns the
    name. A symlink planted at the ring path used to make chmod follow into the victim dir (mode
    rewritten 0755→0700), raw pairs landed inside it, and ls-on-the-link broke the byte bound — a
    link into a synced folder would ship raw audio off the Mac. Now the preflight refuses: exit 2
    before any capture side starts, and the victim — its mode and every file in it — untouched."""
    with Rig() as rig:
        victim = rig.home / "victim"
        victim.mkdir()
        (victim / "keep.txt").write_bytes(b"x" * 24)
        os.chmod(victim, 0o755)
        link = rig.home / "planted-ring"
        os.symlink(victim, link)
        rig.start(EARS_RING_DIR=str(link))
        rc, said = rig.refuse()
        assert rc == 2, (rc, said)
        assert "not ours or not a plain dir" in said, said
        assert victim.stat().st_mode & 0o777 == 0o755, "chmod never followed the link"
        assert [p.name for p in victim.iterdir()] == ["keep.txt"], "nothing wrote into the victim"
        assert (victim / "keep.txt").stat().st_size == 24
        sid = rig.proc.pid
        time.sleep(3)
        stray = subprocess.run(
            ["ps", "-s", str(sid), "-o", "pid="], capture_output=True, text=True
        ).stdout
        assert not stray.strip(), f"stray capture processes: {stray!r}"


def check_ring_dir_defaults_to_tmp_and_honors_the_override():
    """The ring's home is /tmp by the owner's word (vany, 2026-11-19: «пиши хвост в /tmp 0600 — он
    сам убьётся») — the system's own /tmp cleanup IS the retention: the tail survives a crash for
    post-mortem and no stop/uninstall wipe is wanted. The name carries the uid (akira F5: two
    accounts on one Mac never collide). The rig may not touch the real /tmp (nothing outside its
    temporary home), so the default and the ownership check are asserted from the script's own
    text and the override is proven end-to-end: EARS_RING_DIR pointed elsewhere inside the rig
    home, pieces land there with the same 0700/0600 discipline."""
    text = EARS.read_text()
    assert "EARS_RING_DIR:-/tmp/tulpa-ears-ring-$(id -u)" in text, "the uid-suffixed /tmp default"
    assert '[ -O "$RING" ]' in text, "ownership is checked (F5: foreign dir refuses)"
    with Rig() as rig:
        elsewhere = rig.home / "elsewhere-ring"
        rig.transcript("c000001", [("слово", 0, 400)])
        rig.piece("me", "c000001", 3000, 5_000_000, 5_003_000)
        rig.start(EARS_RING_DIR=str(elsewhere))
        rig.lines(2)
        names = {p.name for p in elsewhere.iterdir()}
        assert {"me-c000001.wav", "me-c000001.json"} <= names, sorted(names)
        assert elsewhere.stat().st_mode & 0o777 == 0o700, oct(elsewhere.stat().st_mode & 0o777)
        for p in elsewhere.iterdir():
            assert p.stat().st_mode & 0o777 == 0o600, (p.name, oct(p.stat().st_mode & 0o777))


def check_an_unwritable_ring_dir_refuses_before_capture():
    """A ring that cannot work refuses BEFORE the capture sides start — the same class as the
    bound check: a hot mic with no consumer is the one outcome this script must never produce.
    An unwritable parent and a plain file at the ring path both refuse with exit 2, and the
    session holds nothing 3 s later. (The foreign-OWNER case of F5 cannot be staged by a single
    uid on this box — `[ -O "$RING" ]` is asserted from the script's text in the defaults check,
    and a second-user probe belongs to a Mac.)"""
    for how in ("parent", "file"):
        with Rig() as rig:
            if how == "parent":
                parent = rig.home / "ro"
                parent.mkdir()
                os.chmod(parent, 0o500)
                ring = str(parent / "ring")
            else:
                ring = rig.home / "plain-file-ring"
                pathlib.Path(ring).write_bytes(b"")
            rig.start(EARS_RING_DIR=ring)
            rc, said = rig.refuse()
            assert rc == 2, (how, rc, said)
            assert "ring dir is not ours or not a plain dir" in said, (how, said)
            sid = rig.proc.pid
            time.sleep(3)
            stray = subprocess.run(
                ["ps", "-s", str(sid), "-o", "pid="], capture_output=True, text=True
            ).stdout
            assert not stray.strip(), (how, f"stray capture processes: {stray!r}")


def check_credit_leaks_publish_as_voice():
    """Given music, whisper recites its training-data subtitle credits — six of the eight
    pieces in the 2026-11-19 work-mac capture, word for word. Those pieces are not speech
    in the room: each publishes as <voice N.Ns> with no words payload, so the hub sub
    skips them and nobody wakes (glados 06:19Z). The inputs are the real captures."""
    with Rig() as rig:
        credits = (
            "me-1795069373.json",
            "me-1795069478.json",
            "me-1795069415.json",
            "me-1795069395.json",
            "me-1795069407.json",
            "me-1795069451.json",
        )
        for i, name in enumerate(credits, 1):
            rig.transcript_file(f"c{i:06d}", FIXTURES / name)
            rig.piece("me", f"c{i:06d}", 5000, 1_000_000 * i, 1_000_000 * i + 5000)
        rig.start()
        lines = rig.lines(len(credits) + 1)[1:]
        assert len(lines) == len(credits), lines
        for line in lines:
            assert line.endswith(" [me] <voice 5.0s>"), line
            assert "\t" not in line, f"a leaked credit carries no words: {line}"


def check_real_speech_mentioning_subtitles_passes():
    """The detector matches credit shapes, not the word: real speech about subtitles
    publishes with its text and words (the mandatory near-miss)."""
    with Rig() as rig:
        rig.transcript(
            "c000001",
            [
                (" Я", 0, 300),
                (" делаю", 310, 600),
                (" субтитры", 610, 900),
                (" к", 910, 1000),
                (" роли", 1010, 1200),
            ],
        )
        rig.piece("me", "c000001", 5000, 1_000_000, 1_005_000)
        rig.start()
        _, marker = rig.lines(2)
        text, _, payload = marker.partition("\t")
        assert "субтитры" in text and "<voice" not in text, text
        assert payload == (
            "1000000,1000300,Я;1000310,1000600,делаю;1000610,1000900,субтитры;"
            "1000910,1001000,к;1001010,1001200,роли"
        ), payload


def check_a_repeat_token_loop_is_voice_but_five_is_speech():
    """A decode loop — one token repeated far past any natural stutter — is junk at the
    bound of six; five identical tokens is emphatic speech and publishes. Synthetic
    fixtures: no loop landed in the 2026-11-19 capture, the morning wakes showed the
    shape (ПОДПИШИСЬ repeats)."""
    with Rig() as rig:
        rig.transcript("c000001", [(" ПОДПИШИСЬ", i * 500 + 100, i * 500 + 400) for i in range(9)])
        rig.transcript("c000002", [(" ну", i * 300 + 100, i * 300 + 250) for i in range(5)])
        rig.piece("me", "c000001", 8000, 1_000_000, 1_008_000)
        rig.piece("me", "c000002", 4000, 2_000_000, 2_004_000)
        rig.start()
        _, loop, emphatic = rig.lines(3)
        assert loop.endswith(" [me] <voice 8.0s>"), loop
        assert "\t" not in loop, loop
        assert emphatic.split("[me] ", 1)[1] == (
            "ну ну ну ну ну\t2000100,2000250,ну;2000400,2000550,ну;2000700,2000850,ну;"
            "2001000,2001150,ну;2001300,2001450,ну"
        ), emphatic


def check_real_and_madeup_speech_fixtures_pass_through():
    """them-last.json is real speech at the Mac (a door check); me-1795069355 is fluent
    hallucinated speech with no separating signal measured (p 0.929 against 0.891 real)
    and no leaked-credit shape — both publish as text today, pinned here so the open
    class stays visible instead of silently half-fixed. The empty capture (0 segments)
    keeps its existing <voice> behavior, from the real file."""
    with Rig() as rig:
        rig.transcript_file("c000001", FIXTURES / "them-last.json")
        rig.transcript_file("c000002", FIXTURES / "me-1795069355.json")
        rig.transcript_file("c000003", FIXTURES / "me-1795069444.json")
        rig.piece("them", "c000001", 4000, 1_000_000, 1_004_000)
        rig.piece("them", "c000002", 6000, 2_000_000, 2_006_000)
        rig.piece("me", "c000003", 3000, 3_000_000, 3_003_000)
        rig.start()
        markers = rig.lines(4)[1:]   # the sides race: match by what each line says
        door = next(l for l in markers if "Проверка двери" in l)
        music = next(l for l in markers if "Чтобы приобретать" in l)
        empty = next(l for l in markers if "<voice 3.0s>" in l)
        assert door.startswith("TULPA-EARS-LINE 1000000 1004000 ") and "\t" in door, door
        assert music.startswith("TULPA-EARS-LINE 2000000 2006000 ") and "\t" in music, music
        assert empty.endswith(" [me] <voice 3.0s>") and "\t" not in empty, empty


# The unread verdict (#0454 B1): every <voice N.Ns> piece is told to the tray once, with the rule that
# decided it and its inputs, as TULPA-EARS-DEBUG [<side>] voice-unread:<rule> nt=.. m=.. maxrun=.. ms=..
# — the rule names of tulpa-hear's assemble::Why. A read piece is told nothing.
RULE_PIECES = (
    # name, transcript (tokens or a fixture file), ms, the verdict due (None: read, no record)
    ("c000001", [], 3000, "voice-unread:no-tokens nt=0 m=0 maxrun=0 ms=3000"),
    ("c000002", [("[_BEG_]", 0, 0), ("[_TT_150]", 3000, 3000)], 3000,
     "voice-unread:markers-only nt=2 m=0 maxrun=0 ms=3000"),
    ("c000003", [(" ", 0, 100), ("[_EOT_]", 100, 100)], 2000, "voice-unread:empty-text nt=2 m=1 maxrun=1 ms=2000"),
    ("c000004", FIXTURES / "me-1795069373.json", 5000,
     "voice-unread:credit-leak nt=16 m=14 maxrun=1 ms=5000 family=subtitles-created"),
    ("c000005", [(" ла", i * 100, i * 100 + 90) for i in range(7)], 4000,
     "voice-unread:maxrun nt=7 m=7 maxrun=7 ms=4000"),
    ("c000006", [(" ну", i * 100, i * 100 + 90) for i in range(5)], 4000, None),
    ("c000007", FIXTURES / "jfk.whisper-1.9.6.json", 11000, None),
)


def rule_rig(rig, side="me"):
    for i, (name, src, ms, _) in enumerate(RULE_PIECES, 1):
        if isinstance(src, pathlib.Path):
            rig.transcript_file(name, src)
        else:
            rig.transcript(name, src)
        rig.piece(side, name, ms, 1_000_000 * i, 1_000_000 * i + ms)


def check_every_voice_piece_tells_its_rule():
    """Each rule the assembler can decide, one piece each, plus two read pieces: one verdict
    record per <voice> line, in order, naming the rule and its inputs; none for a read line;
    no text of a piece in any record."""
    with Rig() as rig:
        rule_rig(rig)
        rig.start()
        lines = rig.lines(len(RULE_PIECES) + 1)[1:]
        due = [v for _, _, _, v in RULE_PIECES if v]
        assert rig.debug == [f"TULPA-EARS-DEBUG [me] {v}" for v in due], rig.debug
        for line, (_, _, ms, v) in zip(lines, RULE_PIECES):
            voice = line.endswith(f" [me] <voice {ms / 1000:.1f}s>")
            assert voice == (v is not None), (line, v)
        for rec in rig.debug:
            for word in ("Субтитры", "DimaTorzok", "ла", "ну"):
                assert word not in rec, rec


def check_a_credit_line_costs_only_its_words():
    """#0454 B1 item 1b: a piece of speech with a signed credit glued to its end publishes the
    speech, word timings and all, in its TULPA-EARS-LINE, and leaves one record naming what it
    dropped (family and count, no text) on the DEBUG channel only; the line never carries it."""
    with Rig() as rig:
        rig.transcript("c000001", [(" Привет", 0, 300), (" мир.", 300, 600), (" Субтитры", 700, 900),
                                   (" сделал", 900, 1100), (" Dima", 1100, 1300), ("Torzok", 1300, 1500)])
        rig.piece("me", "c000001", 2000, 1_000_000, 1_002_000)
        rig.start()
        _, line = rig.lines(2)
        assert line.split("[me] ", 1)[1] == "Привет мир.\t1000000,1000300,Привет;1000300,1000600,мир.", line
        assert rig.debug == ["TULPA-EARS-DEBUG [me] credit-dropped n=1 ms=2000 family=subtitles-made"], rig.debug
        assert "credit-dropped" not in line and "Субтитры" not in line and "Torzok" not in line, line


def check_a_held_turns_unread_middle_tells_its_rule_once():
    """The mid-hold <voice> of a merged turn (read, unread, read) has its one record; the
    merged line is as before."""
    with Rig() as rig:
        rig.transcript("c000001", [("привет", 0, 500), (" как", 700, 1100)])
        rig.transcript("c000002", [("[_BEG_]", 0, 0)])
        rig.transcript("c000003", [(" мир", 200, 900)])
        rig.piece("me", "c000001", 6000, 1_000_000, 1_006_000, cut=True)
        rig.piece("me", "c000002", 4000, 1_012_400, 1_016_400, cut=True)
        rig.piece("me", "c000003", 5000, 1_017_000, 1_022_000)
        rig.start()
        _, marker = rig.lines(2)
        assert marker.split("[me] ", 1)[1].split("\t")[0] == "привет как<pause 6.4s><voice 4.0s><pause 0.6s>мир", marker
        assert rig.debug == ["TULPA-EARS-DEBUG [me] voice-unread:markers-only nt=1 m=0 maxrun=0 ms=4000"], rig.debug


def check_a_failed_assembler_is_voice_with_its_rule():
    """whisper's transcript the assembler cannot read (awk fails): the piece is <voice> as
    before, and its record says assembler-failed with awk's exit code."""
    if os.geteuid() == 0:
        raise AssertionError("run as a user: root reads a mode-000 file")
    with Rig() as rig:
        rig.transcript("c000001", [(" да", 0, 300)])
        rig.unreadable("c000001")
        rig.piece("them", "c000001", 4000, 1_000_000, 1_004_000)
        rig.start()
        _, line = rig.lines(2)
        assert line.endswith(" [them] <voice 4.0s>"), line
        assert len(rig.debug) == 1, rig.debug
        rec = rig.debug[0]
        assert rec.startswith("TULPA-EARS-DEBUG [them] voice-unread:assembler-failed rc="), rec
        assert rec.endswith(" ms=4000") and " rc=0 " not in rec, rec


def check_a_missing_verdict_is_said_as_unknown():
    """The loop never stays silent on a piece: an assembler that prints no verdict line (a
    later edit breaking the contract, staged by cutting its three verdict prints out of a copy
    of the script) gives voice-unread:unknown for every piece — and the same TULPA-EARS-LINE bytes."""
    src = EARS.read_text()
    cut = src.replace('    printf "voice-unread:', '    if (0) printf "voice-unread:', 1)
    cut = cut.replace('  else print "read"', '  else if (0) print "read"', 1)
    cut = cut.replace('  if (nleak) printf "credit-dropped', '  if (0) printf "credit-dropped', 1)
    assert 'print "read"' in src and cut.count('if (0) print') == 3, "the cut missed"
    out = {}
    for label, text in (("cut", cut), ("real", src)):
        with Rig() as rig:
            script = rig.home / "ears-capture.sh"
            script.write_text(text)
            rig.script = script
            rule_rig(rig)
            rig.start()
            lines = rig.lines(len(RULE_PIECES) + 1)[1:]
            out[label] = ([" ".join(l.split(" ")[:3] + l.split(" ")[4:]) for l in lines], rig.debug)
    assert out["cut"][0] == out["real"][0], out
    assert len(out["cut"][1]) == len(RULE_PIECES), out["cut"][1]
    for rec, (_, _, ms, _) in zip(out["cut"][1], RULE_PIECES):
        assert rec == f"TULPA-EARS-DEBUG [me] voice-unread:unknown ms={ms}", rec


# D24 (#0454 B1): the loop takes a verdict only from a line that IS one. The assembler's output is
# P (the piece), V (the verdict), H (heard, after a dropped credit). A copy of the script passes it
# through an awk that prints the lines in ORDER, by kind (a kind the piece lacks is skipped; X is a
# line of no kind). Three pieces each run: A credit-dropped (P V H), B read (P V), C credit-leak
# (P V H, its H the credit's text). The verdict due per piece, written by hand: "unknown" whenever
# the second line is not a verdict, None for read (no record).
VA = "credit-dropped n=1 ms=2000 family=subtitles-made"
VC = "voice-unread:credit-leak nt=16 m=14 maxrun=1 ms=5000 family=subtitles-created"
UNKNOWN = {"A": "voice-unread:unknown ms=2000", "B": "voice-unread:unknown ms=3000",
           "C": "voice-unread:unknown ms=5000"}
VERDICT_ORDERS = (
    # order, A, B, C
    ("PVH", VA, None, VC),                    # the contract: the control
    ("VH", "unknown", "unknown", "unknown"),  # piece removed
    ("PH", "unknown", "unknown", "unknown"),  # verdict removed
    ("PV", VA, None, VC),                     # heard removed
    ("VPH", "unknown", "unknown", "unknown"),  # piece and verdict swapped
    ("HVP", VA, "unknown", VC),               # piece and heard swapped (B: V P)
    ("PHV", "unknown", None, "unknown"),      # verdict and heard swapped (B: P V)
    ("PXVH", "unknown", "unknown", "unknown"),  # another line before the verdict
    ("PVHX", VA, None, VC),                   # another line after
)
PIECE_WORDS = ("Привет", "мир", "Субтитры", "сделал", "создавал", "Dima", "Torzok", "да")
CALL = 'tp=$(awk -v epoch="$t0" -v ms="$ms" "$ASSEMBLE" "$W.json"); rc=$?'
REORDER = ("""awk -v order=%s '{ l[NR] = $0 } END { for (i = 1; i <= length(order); i++) {"""
           """ c = substr(order, i, 1); k = index("PVH", c);"""
           """ if (c == "X") print "rig other line"; else if (k in l) print l[k] } }'""")


def check_a_verdict_comes_only_from_a_verdict_line():
    """Each line of the assembler's output removed in turn, and swapped in pairs, and another line
    put in: the record due per piece is present (the records equal the hand-written list, in
    order), and no word of any piece is in any record (P5)."""
    src = EARS.read_text()
    assert src.count(CALL) == 1, "the awk call moved"
    bad = []
    for order, *due in VERDICT_ORDERS:
        text = src.replace(CALL, CALL.replace('"$W.json")', '"$W.json" | %s)' % (REORDER % order)))
        with Rig() as rig:
            script = rig.home / "ears-capture.sh"
            script.write_text(text)
            rig.script = script
            rig.transcript("c000001", [(" Привет", 0, 300), (" мир.", 300, 600), (" Субтитры", 700, 900),
                                       (" сделал", 900, 1100), (" Dima", 1100, 1300), ("Torzok", 1300, 1500)])
            rig.piece("me", "c000001", 2000, 1_000_000, 1_002_000)
            rig.transcript("c000002", [(" да", 0, 300)])
            rig.piece("me", "c000002", 3000, 2_000_000, 2_003_000)
            rig.transcript_file("c000003", FIXTURES / "me-1795069373.json")
            rig.piece("me", "c000003", 5000, 3_000_000, 3_005_000)
            rig.start()
            rig.lines(4)
            want = [f"TULPA-EARS-DEBUG [me] {UNKNOWN[p] if v == 'unknown' else v}"
                    for p, v in zip("ABC", due) if v is not None]
            if rig.debug != want:
                bad.append(f"{order}: records {rig.debug}, due {want}")
            bad += [f"{order}: piece text in {rec!r}" for rec in rig.debug for w in PIECE_WORDS if w in rec]
    off = sorted({b.split(":", 1)[0] for b in bad})
    assert not bad, f"{len(off)} of {len(VERDICT_ORDERS)} orders off ({' '.join(off)}); " + "; ".join(bad)


def check_file_mode_tells_the_rule_on_stderr():
    """Without TULPA_LINES_STDOUT the verdict goes to stderr, the same record; the transcript
    on stdout and in the call file carries none."""
    with Rig() as rig:
        rig.transcript("c000001", [("[_BEG_]", 0, 0)])
        rig.piece("me", "c000001", 3000, 1_000_000, 1_003_000)
        rig.start(TULPA_LINES_STDOUT="0")
        _, line = rig.lines(2)
        assert line.endswith(" [me] <voice 3.0s>"), line
        assert rig.debug == [], f"no record on stdout: {rig.debug}"
        os.killpg(os.getpgid(rig.proc.pid), signal.SIGTERM)
        _, err = rig.proc.communicate(timeout=10)
        err = err.decode("utf-8", "replace")
        assert "TULPA-EARS-DEBUG [me] voice-unread:markers-only nt=1 m=0 maxrun=0 ms=3000\n" in err, err
        call = (rig.state / "call-me.txt").read_text()
        assert "TULPA-EARS-DEBUG" not in call and "<voice 3.0s>" in call, call


# The text tools the capture script runs (the r2k enumeration, REPORT): each one reads, counts,
# compares or rewrites piece text or a listing, and each must run in the C locale the assembler
# and the Rust parity were reviewed in — the Mac's awk counts characters under a UTF-8 LANG
# (#0454 B1 1b, install1b/bsdawk: 295 of 3112 pieces differ, 46 lose speech). printf and the
# shell's own ${..}/case matching are builtins: they read the shell's LC_ALL, which a PATH stub
# cannot see, but the same export covers them.
TEXT_TOOLS = ("awk", "sed", "tr", "grep", "sort", "head", "ls", "date")
LOCALE_VARS = ("LANG", "LC_ALL", "LC_CTYPE", "LC_COLLATE", "LC_NUMERIC", "LC_MESSAGES", "LC_TIME", "LC_MONETARY")
# The parent envs the tray or a hand run may pass (r2k P-L1): none, a UTF-8 LANG in three forms,
# LC_CTYPE alone, and LC_ALL itself set to UTF-8.
PARENT_LOCALES = (
    {},
    {"LANG": "en_US.UTF-8"},
    {"LANG": "C.UTF-8"},
    {"LANG": "ru_RU.UTF-8"},
    {"LC_CTYPE": "en_US.UTF-8"},
    {"LC_ALL": "en_US.UTF-8"},
)
LOCALE_STUB = """#!/bin/sh
printf '%s LC_ALL=%s LANG=%s LC_CTYPE=%s\\n' {name} "${{LC_ALL-<unset>}}" "${{LANG-<unset>}}" "${{LC_CTYPE-<unset>}}" >> "$TEST_LOCALE_LOG"
exec {real} "$@"
"""


def check_every_text_tool_runs_in_the_c_locale():
    """Whatever locale the parent passes, every text tool the script starts sees LC_ALL=C: stubs
    first on the script's PATH log their name and locale, then exec the real tool. The log must
    name every tool of TEXT_TOOLS (the scene reaches each: the prompt pairs, the version line, the
    listing, the segment json, a held turn's pause, the published line's clock) and every record
    must carry LC_ALL=C."""
    bad = []
    for parent in PARENT_LOCALES:
        with Rig() as rig:
            wrap = rig.home / "locale-bin"
            wrap.mkdir()
            for name in TEXT_TOOLS:
                real = shutil.which(name, path="/usr/bin:/bin")
                assert real, f"no real {name} in /usr/bin:/bin"
                stub = wrap / name
                stub.write_text(LOCALE_STUB.format(name=name, real=real))
                stub.chmod(0o755)
            log = rig.home / "locale.log"
            rig.transcript("c000001", [(" Привет", 0, 300), (" мир", 300, 600)])
            rig.transcript("c000002", [(" как", 0, 300), (" дела", 300, 600)])
            rig.piece("me", "c000001", 1000, 1_000_000, 1_001_000, cut=True)
            rig.piece("me", "c000002", 1000, 1_002_000, 1_003_000)
            rig.start(
                unset=LOCALE_VARS,
                EARS_PATH=f"{wrap}:{rig.bin}:/usr/bin:/bin",
                EARS_PROMPT="Тулпа, агент",
                TEST_LOCALE_LOG=str(log),
                **parent,
            )
            _, line = rig.lines(2)
            assert line.split("[me] ", 1)[1].startswith("Привет мир<pause 1.0s>как дела\t"), line
            records = log.read_text().splitlines() if log.exists() else []
            seen = {r.split(" ", 1)[0] for r in records}
            missing = [t for t in TEXT_TOOLS if t not in seen]
            if missing:
                bad.append(f"{parent}: never ran {missing}")
            off = sorted({r for r in records if " LC_ALL=C " not in r})
            if off:
                bad.append(f"{parent}: {len(off)} kinds of record without LC_ALL=C, e.g. {off[:3]}")
    assert not bad, "; ".join(bad)


CHECKS = [
    check_dropped_phrases_are_told_to_the_tray,
    check_bad_ring_bound_refuses_before_any_capture,
    check_ring_is_excluded_from_time_machine,
    check_a_symlinked_ring_dir_refuses_and_touches_nothing,
    check_ring_dir_defaults_to_tmp_and_honors_the_override,
    check_an_unwritable_ring_dir_refuses_before_capture,
    check_version_mismatch_refuses,
    check_verified_versions_run_a_real_capture,
    check_version_silence_refuses,
    check_held_turn_keeps_its_midhold_voice,
    check_hold_bound_publishes_midphrase,
    check_hold_far_from_the_bound_merges,
    check_assembler_drops_whisper_markers_and_merges_pieces,
    check_a_phrase_of_only_markers_is_voice,
    check_ring_keeps_consumed_pairs_and_drops_oldest,
    check_ring_survives_a_restart,
    check_ring_holds_the_selftagged_piece_too,
    check_credit_leaks_publish_as_voice,
    check_real_speech_mentioning_subtitles_passes,
    check_a_repeat_token_loop_is_voice_but_five_is_speech,
    check_real_and_madeup_speech_fixtures_pass_through,
    check_every_voice_piece_tells_its_rule,
    check_a_credit_line_costs_only_its_words,
    check_a_held_turns_unread_middle_tells_its_rule_once,
    check_a_failed_assembler_is_voice_with_its_rule,
    check_a_missing_verdict_is_said_as_unknown,
    check_a_verdict_comes_only_from_a_verdict_line,
    check_file_mode_tells_the_rule_on_stderr,
    check_every_text_tool_runs_in_the_c_locale,
]


def main():
    failed = 0
    for check in CHECKS:
        try:
            check()
        except Exception as error:  # noqa: BLE001 — a rig reports, never raises
            failed += 1
            print(f"fail {check.__name__}: {error}")
        else:
            print(f"ok   {check.__name__}")
    print(f"{len(CHECKS) - failed}/{len(CHECKS)} passed")
    raise SystemExit(1 if failed else 0)


if __name__ == "__main__":
    main()
