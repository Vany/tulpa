#!/bin/sh
# ears-capture.sh — Tulpa's ears ([ears] command): hears the room and the call.
# 'me' = the microphone (avfoundation, by name in EARS_MIC), 'them' = the system audio
# (ScreenCaptureKit via tulpa-sysaudio). tulpa-segment cuts whole utterances on pauses — the close
# is 2 s of quiet, and ANY voice activity resets it (an unrecognised "эээ" holds the thought open,
# by order of 2026-11-18: the pause, not the words, ends the turn) — whisper-cli transcribes each
# phrase whole, and the phrase leaves as ONE line on this script's stdout:
#   TULPA-EARS-LINE <t0> <t1> <HH:MM:SS> [me|them] [tag] text [intonation: rising|falling]<TAB>words
# (t0/t1: the utterance's own span, epoch ms). The tags live inside the stream (the owner, 2026-11-18):
# a mid-phrase gap of half a second or more is <pause 1.8s> in the text, an utterance the gate
# heard but whisper could not read is <voice 2.1s> alone — voice activity, his not-done-thinking
# signal — and after the line's one TAB come the phrase's word timings, <t0>,<t1>,<word> records
# separated by ';' in epoch ms, for whoever downstream decides the turn. Each unread piece also
# leaves one TULPA-EARS-DEBUG record naming the rule that made it <voice> (unread() below). The
# tray tees stdout to its side log, parses the markers and sends them over its link, and gates our
# own playback's echo in memory — nothing in the ears' path is a file anymore (order of 2026-11-18).
#
# Everything the ears need is beside this script (an app bundle's Resources: helpers in ../MacOS)
# or under its own state dir EARS_STATE (default ~/.cache/tulpa-ears); no shared folder is read or
# written (order of 2026-11-18: no dependence on any sync). whisper-cli and its models stay local
# machine state (EARS_MODELS, default ~/.cache/calltx) — a missing piece is refused loudly below,
# never silent deafness.
#
# With TULPA_LINES_STDOUT unset the old behaviour stays — transcript files under the state dir,
# tailed by an older tray — so a rig can move to the bundle one Mac at a time.
#
# Envs: EARS_MIC EARS_GAIN EARS_LANG (ru) EARS_PROMPT (whisper vocabulary hint; its word pairs are
# the echo-junk filter) EARS_TAG (speaker tagger, optional; prints "self" or a tag for one wav)
# EARS_SEGMENTER=fixed4 (back to fixed 4 s windows, no helper) EARS_CLOSE_MS (2000) EARS_MAX_MS
# (60000) EARS_HOLD_MAX_MS (300000: held-speech bound, above) EARS_RING_MAX_MB (8: raw ring's
# byte bound, below) EARS_RING_DIR (/tmp/tulpa-ears-ring-<uid>: the ring's home, below) EARS_MODELS
# EARS_STATE
# TULPA_BUNDLE_BIN (helpers' dir, default ../MacOS beside this script) TULPA_LINES_STDOUT=1
# (the tray sets it) EARS_PATH (the PATH the script runs its helpers on, default the Homebrew
# and system dirs; a rig overrides it — the default is the production hermeticity).
export PATH="${EARS_PATH:-/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin}"
# Every text tool below runs in the C locale, whatever the tray or a hand run passes: the assembler,
# the Rust parity and the rig are written for and reviewed in bytes, and the Mac's awk counts
# characters under a UTF-8 LANG (#0454 B1 1b, 2026-11-26: the tray passes LANG=en_US.UTF-8, and 295
# of 3112 pieces came out different, 46 of them losing speech). LC_ALL beats LANG and every LC_*.
# The helpers need no UTF-8 locale: whisper-cli and ffmpeg call no setlocale, tulpa-segment and
# tulpa-sysaudio write bytes. The shell itself (printf, ${..} and case matching) follows it too.
export LC_ALL=C

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
BIN="${TULPA_BUNDLE_BIN:-$HERE/../MacOS}"
SYSAUDIO="$BIN/tulpa-sysaudio"
SEGMENT="$BIN/tulpa-segment"
M="${EARS_MODELS:-$HOME/.cache/calltx}"
MODEL="$M/ggml-large-v3-turbo-q5_0.bin"
VAD="$M/ggml-silero-v5.1.2.bin"
MIC="${EARS_MIC:-:MacBook Pro Microphone}"   # by name: one capture script, any Mac's mic
GAIN="${EARS_GAIN:-8dB}"
D="${EARS_STATE:-$HOME/.cache/tulpa-ears}"
TAG="${EARS_TAG:-}"
# The close is the end-of-phrase pause (2 s); max keeps a monologue from never shipping.
SEGARGS="--close-ms ${EARS_CLOSE_MS:-2000} --max-ms ${EARS_MAX_MS:-60000}"
# How much held speech a never-pausing turn may pile up before it ships anyway, mid-phrase
# (akira, 2026-11-18: the owner's "only the pause ends a turn" must not deadlock against a
# caller who never pauses). What follows the bound is the next line.
HOLDMAX="${EARS_HOLD_MAX_MS:-300000}"

if [ "${EARS_SEGMENTER:-pause}" != fixed4 ] && [ -x "$SEGMENT" ]; then CUT=pauses; NEED=1; else CUT=4s; NEED=2; fi   # NEED: with fixed windows a segment file in progress holds the newest name

# Preflight: name every missing piece once, plainly — a deaf ear must be loud, on stdout (the side
# log) and stderr both, and the ears refuse rather than run half-hearing.
miss=""
command -v ffmpeg >/dev/null || miss="$miss ffmpeg (brew install ffmpeg)"
if command -v whisper-cli >/dev/null; then
  # Presence is not enough: the assembler below parses whisper.cpp's -ojf layout by TAB depth,
  # and another version may re-indent it — every phrase would degrade to <voice N.Ns> (heard,
  # unread) while the pipeline looks healthy. A wrong version is a loud refusal, not silent
  # deafness (akira, 2026-11-18). OJF_VERSIONS is the one list of versions whose layout the
  # assembler was verified against (1.9.6: the same TAB depths and keys as 1.9.5, 2026-11-25);
  # the whole first line must read "whisper.cpp version: <one of them>", so 1.9.50, 1.9.6-rc1
  # or any other shape of line refuse too.
  OJF_VERSIONS="1.9.5 1.9.6"
  wv=$(whisper-cli --version 2>&1 | head -n 1)
  ok=""
  for v in $OJF_VERSIONS; do
    [ "$wv" = "whisper.cpp version: $v" ] && ok=1
  done
  [ -n "$ok" ] || miss="$miss whisper-cli $OJF_VERSIONS (the assembler parses those versions' -ojf layout; this one says: ${wv:-nothing})"
else
  miss="$miss whisper-cli (brew install whisper-cpp)"
fi
if [ "$CUT" = pauses ]; then
  [ -x "$SYSAUDIO" ] || miss="$miss $SYSAUDIO (build the app bundle: scripts/bundle-macos.sh)"
  [ -x "$SEGMENT" ] || miss="$miss $SEGMENT (build the app bundle: scripts/bundle-macos.sh)"
fi
[ -f "$MODEL" ] || miss="$miss model $MODEL (whisper.cpp's ggml-large-v3-turbo q5_0)"
[ -f "$VAD" ]   || miss="$miss VAD model $VAD (whisper.cpp's ggml-silero v5.1.2)"
if [ -n "$miss" ]; then
  echo "ears-capture: refusing to run, missing:$miss"
  echo "ears-capture: refusing to run, missing:$miss" >&2
  exit 1
fi

# The ring's byte bound must be a whole number of MiB, 1..1024, with no leading zero — validated
# AND computed here, before anything starts (akira, 2026-11-19, F1 + the 16:35Z gap: `08`/`09`
# pass a digits-only guard but are invalid octal in the arithmetic, so the shell died after the
# capture sides were already rolling — a hot mic with no consumer; `010` silently means 8; a huge
# value overflows negative and silently empties the ring). A leading zero is refused outright, the
# cap catches overflow, and computing RING_MAX in preflight means any arithmetic failure lands
# before the first `&`. 0 is refused with the same case — the ring is the owner's ordered policy,
# not a switch to turn off by typing 0.
case ${EARS_RING_MAX_MB:-8} in ''|*[!0-9]*|0*) echo "ears-capture: EARS_RING_MAX_MB must be a whole number of MiB, 1..1024, no leading zero" >&2; exit 2;; esac
[ "${EARS_RING_MAX_MB:-8}" -le 1024 ] || { echo "ears-capture: EARS_RING_MAX_MB must be at most 1024" >&2; exit 2; }
# The ring's home is /tmp by the owner's word (vany, 2026-11-19, verbatim: «пиши хвост в /tmp 0600 —
# он сам убьётся»): the system's own /tmp cleanup IS the retention — the tail survives a crash for
# post-mortem, never ships off the Mac, and no stop/uninstall wipe is wanted. The name carries the
# uid so two accounts on one Mac never collide (akira F5, 2026-11-19: /tmp is writable by every
# local user and the first creator owns the name). EARS_RING_DIR overrides it (the test rig points
# it inside its temporary home, never at the real /tmp).
RING="${EARS_RING_DIR:-/tmp/tulpa-ears-ring-$(id -u)}"
RING_MAX=$(( ${EARS_RING_MAX_MB:-8} * 1024 * 1024 ))

rm -rf "$D/them" "$D/me"; mkdir -p "$D/them" "$D/me"; : > "$D/sysaudio.err"
# The ring is outside the state dir, so the wipe above cannot touch it — the last ~minute survives
# a restart by construction (new segment names start from the clock, so no collision either).
# /tmp is writable by every local user, and the first one to create a fixed name owns it: a planted
# SYMLINK made chmod follow into a victim dir (its mode rewritten 0755→0700), raw pairs land
# inside it, and ls-on-the-link broke the byte bound entirely — a link into a synced folder would
# ship raw audio off the Mac, exactly what the owner's policy forbids (akira F5, 2026-11-19). So
# the ring must be a plain dir we OWN, checked before any capture starts and on every use; a ring
# that cannot be trusted refuses (preflight) or drops its piece loudly (mid-run), never writes
# through a link.
ring_ready() {  # true when the ring dir is a plain dir we own — created on first use
  { [ -d "$RING" ] && [ ! -L "$RING" ] && [ -O "$RING" ]; } \
    || { mkdir -p "$RING" 2>/dev/null || return 1; tmutil addexclusion "$RING" 2>/dev/null || true; }
  [ -d "$RING" ] && [ ! -L "$RING" ] && [ -O "$RING" ] && chmod 700 "$RING"
}
ring_ready || { echo "ears-capture: ring dir is not ours or not a plain dir: $RING" >&2; exit 2; }
# Time Machine excludes /tmp by default; the addexclusion stays as named insurance for this exact
# path (akira's F2 class: raw audio must never leave the Mac on a backup). Sticky by path, no root
# needed; absent off macOS, hence the guard. `tmutil isexcluded` on the ring path is an
# install-slice check.
tmutil addexclusion "$RING" 2>/dev/null || true
# Segment numbers start from the clock, so a restarted side never reuses a chunk name the loop is reading.
SEG="-f segment -segment_time 4 -reset_timestamps 1"
( while :; do
    if [ "$CUT" = pauses ]; then
      "$SYSAUDIO" 2>>"$D/sysaudio.err" | "$SEGMENT" $SEGARGS --dir "$D/them" --start "$(date +%s)000" 2>>"$D/segment-them.log"
    else
      "$SYSAUDIO" 2>>"$D/sysaudio.err" | ffmpeg -hide_banner -loglevel error -f s16le -ar 16000 -ac 1 -i - $SEG -segment_start_number "$(date +%s)" "$D/them/c%06d.wav"
    fi
    sleep 30   # sysaudio dies with "no display" while the display sleeps; it comes back here
  done ) &
T=$!
# 2026-11-14: the mic delivers 48 kHz; without resampling with async and re-stamping, a 4 s segment
# held ~1.2 s of audio (2/3 of speech lost). Keep this -af in every copy.
if [ "$CUT" = pauses ]; then
  ( ffmpeg -hide_banner -loglevel info -nostdin -f avfoundation -i "$MIC" -af "aresample=16000:async=1000,asetpts=N/SR/TB,volume=$GAIN,alimiter=limit=0.95" -ac 1 -ar 16000 -f s16le - 2>"$D/mic.log" | "$SEGMENT" $SEGARGS --dir "$D/me" --start "$(date +%s)000" 2>>"$D/segment-me.log" ) &
else
  ffmpeg -hide_banner -loglevel info -nostdin -f avfoundation -i "$MIC" -af "aresample=16000:async=1000,asetpts=N/SR/TB,volume=$GAIN,alimiter=limit=0.95" -ac 1 -ar 16000 $SEG -segment_start_number "$(date +%s)" "$D/me/c%06d.wav" 2>"$D/mic.log" &
fi
F=$!

# The prompt's adjacent word pairs, one per line: whisper echoes its vocabulary back on silence,
# and a transcript that carries a pair is that echo, not speech. No prompt: an empty list, matching
# nothing.
printf '%s' "${EARS_PROMPT-}" | tr -s ' ' | awk -F', ' 'NF > 1 {for (i = 1; i < NF; i++) print $i", "$(i+1)}' > "$D/prompt-pairs"

# Takes whisper-cli's -ojf JSON on stdin and prints one line: the phrase's text with <pause N.Ns>
# tags at gaps of half a second or more, a TAB, and the word timings as <t0>,<t1>,<word> records
# separated by ';' (epoch ms, the utterance's own t0 added in) — the tags-inside-the-stream shape
# of 2026-11-18. whisper's own marker tokens ([_BEG_], [_TT_N]…) are dropped before anything;
# subword pieces merge into words (a piece with a leading space begins one). A phrase whisper
# could not read prints <voice N.Ns> alone with no TAB; a token carrying ';' or a TAB (the wire's
# own separators) keeps the text and loses the whole timing payload, never half of it. The JSON is
# whisper.cpp's writer in the OJF_VERSIONS above: TAB-indented, one key per line, token keys at
# five TABs, their offsets' numbers at six.
ASSEMBLE='
BEGIN { nt = 0; ns = 0; tables() }
{
  n = 0
  while (substr($0, n + 1, 1) == "\t") n++
  line = substr($0, n + 1)
  if (n == 3 && line ~ /^"tokens": \[/) { intok = 1; ns++; txt = ""; from = ""; to = ""; next }
  if (intok && n == 3 && line ~ /^\]/) { intok = 0; next }
  if (!intok) next
  if (n == 4 && line ~ /^},?$/) {
    if (txt != "" && from != "" && to != "" && from <= to) { nt++; tk[nt] = txt; ta[nt] = from; tb[nt] = to; tg[nt] = ns }
    txt = ""; from = ""; to = ""
    next
  }
  if (n == 5 && line ~ /^"text": "/) {
    s = line
    sub(/^"text": "/, "", s)
    sub(/",?$/, "", s)
    gsub(/\\"/, "\001", s)
    gsub(/\\\\/, "\\", s)
    gsub(/\001/, "\"", s)
    txt = s
    next
  }
  if (n == 6 && line ~ /^"from": [0-9]+/) { s = line; sub(/^"from": /, "", s); sub(/,$/, "", s); from = s + 0; next }
  if (n == 6 && line ~ /^"to": [0-9]+/) { s = line; sub(/^"to": /, "", s); sub(/,$/, "", s); to = s + 0; next }
}
END {
  # The markers whisper adds itself — [_BEG_], [_TT_39], [_EOT_] — are not speech: dropped
  # whole, before the gaps are measured, so a pause across one still counts from real audio
  # time (2026-11-18, the first live 34d7441 line: they leaked into the text and the words).
  # A token (or subword piece) repeated many times in a row is a decode loop, not speech.
  # The bound is 6: natural stutters and emphatic repeats stay under it, whisper music
  # loops run to dozens (the ПОДПИШИСЬ repeats, work-mac wakes 2026-11-19). Pinned from
  # both sides by the rig: five identical tokens still publish as words. Counted over every
  # text token, before the segment loops below are dropped.
  m = 0; prev = ""; runlen = 0; maxrun = 0
  for (i = 1; i <= nt; i++) {
    if (tk[i] ~ /^\[_.*\]$/) continue
    m++; uk[m] = tk[i]; ua[m] = ta[i]; ub[m] = tb[i]; ug[m] = tg[i]
    if (tk[i] == prev) runlen++; else { runlen = 1; prev = tk[i] }
    if (runlen > maxrun) maxrun = runlen
  }
  mt = m   # the text tokens, for the verdict below: m is reused for the kept ones
  # A credit costs only its own line (#0454 B1, 2026-11-25: one DimaTorzok credit at the end of a
  # real 59.8 s turn made all of it <voice>). The text tokens are cut into the lines of
  # assemble.rs lines(), and each line credit_leak names is dropped alone, text and timings (a
  # silence filler only when every line is a credit or a filler: beside a real word it is speech);
  # the loop cut below runs on what is left, as in assemble.rs piece(). A line ends at every
  # whisper segment bound (every real credit is a whole segment of its own), at a gap of
  # 500 ms or more; before a word (a token beginning with a space) after a token ending a
  # sentence. A signed credit is a line of its own wherever it sits: from its Субтитр word
  # (one or two words before the name) or the name itself, to the end of the name word. A line
  # that opens with a credit ends after its own words (own_end) when a word with a letter follows:
  # those words are speech, and the credit is flagged in the verdict (#0454 B1 P6). Only a line
  # that opens at the start, a segment bound or a signed credit bound (the signature is a
  # hallucination token wherever it sits, #0454 B1 D12) can be a credit (cut 1); after a sentence
  # end, a gap or the own words of a credit inside a segment (cut 2) credit words are speech, as the
  # base matched a credit only where its text began (#0454 B1 D8 prime, assemble.rs lines).
  # An initial dot ends no sentence (#0454 B1 D17: Редактор субтитров И. Петрова is one credit).
  # General, not scoped to a name (D23): with every stop cut removed, 3000 pieces without a credit
  # came out the same (r2h), so a stop cut reaches only the own words of a credit.
  nw = 0
  for (j = 1; j <= m; j++) {
    if (j == 1 || substr(uk[j], 1, 1) == " ") { nw++; wb[nw] = j; wx[nw] = "" }
    we[nw] = j; wx[nw] = wx[nw] uk[j]; wn[j] = nw
  }
  for (j = 1; j <= m; j++) cut[j] = 0
  for (j = 2; j <= m; j++) {
    if (ug[j] != ug[j-1]) { cut[j] = 1; continue }
    s = uk[j-1]; sub(/[ \t\n\r\f]+$/, "", s)
    if (substr(uk[j], 1, 1) == " " && (s ~ /[.!?]$/ || s ~ /…$/) && name_kind(wx[wn[j-1]], 0) != 1) { cut[j] = 2; continue }
    if (ua[j] >= ub[j-1] && ua[j] - ub[j-1] >= 500) cut[j] = 2
  }
  for (w = 1; w <= nw; w++) {
    if (!index(norm(wx[w]), SIGN)) continue
    f = w
    for (q = (w > 2 ? w - 2 : 1); q < w; q++) if (index(norm(wx[q]), SIGNBY) == 1) { f = q; break }
    cut[wb[f]] = 1; cut[we[w] + 1] = 1
  }
  for (j = 1; j <= m; j = q) {
    for (q = j + 1; q <= m && !cut[q]; q++) ;
    e = cut[j] == 2 ? 0 : own_end(j, q)
    if (e > j && e < q) { cut[e] = 2; flag[j] = 1 }
  }
  nl = 0; real = 0
  for (j = 1; j <= m; j = q) {
    s = uk[j]
    for (q = j + 1; q <= m && !cut[q]; q++) s = s uk[q]
    sub(/^ +/, "", s); sub(/ +$/, "", s)
    nl++; lj[nl] = j; lq[nl] = q; lk[nl] = cut[j] == 2 ? "" : credit_leak(s)
    if (s != "" && lk[nl] == "") real = 1
  }
  nleak = 0; fam = ""; fams = ""; nflag = 0; ffams = ""
  for (l = 1; l <= nl; l++) {
    if ((lf = lk[l]) != "" && (!real || lf != "silence-filler")) {
      nleak++
      if (fam == "") fam = lf
      if (index("," fams ",", "," lf ",") == 0) fams = fams (fams == "" ? "" : ",") lf
      if (flag[lj[l]]) {
        nflag++
        if (index("," ffams ",", "," lf ",") == 0) ffams = ffams (ffams == "" ? "" : ",") lf
      }
    }
  }
  for (j = 1; j <= m; j++) { ok[j] = uk[j]; oa[j] = ua[j]; ob[j] = ub[j]; og[j] = ug[j] }
  # What the piece says with no credit dropped (said(0)): the loop judges prompt echo on it before the
  # credit rule, which compares against the prompt itself and is the more specific rule (#0454 B1 D7,
  # assemble.rs Piece.heard). Printed only when a credit was dropped; otherwise it is the piece.
  if (nleak) { said(0); heard = why != "" ? sprintf("<voice %.1fs>", ms / 1000) : text }
  said(1)
  # The one place a piece is judged unread, and why. The second line is the verdict the loop
  # reports (never publishes): "read", or voice-unread:<rule> with its inputs, in the
  # rule names of assemble::Why in tulpa-hear (#0454 B1: a 57.5 s turn went <voice> and nothing
  # said why). No text of the piece goes into it; credit-leak names its family by label.
  # Credit-leak: lines dropped and nothing but spaces left (its family: the first dropped line).
  # A piece that kept words says what it dropped instead of read: credit-dropped n=<lines>
  # ms=.. family=<each family once, in order>. Either line ends in flagged=<n> flagged-family=..
  # when credits went on in words the list does not cover (kept as speech, #0454 B1 P6), and
  # in nothing else then: assemble.rs flag_fields. When a credit was dropped, a third line follows:
  # the text with none dropped (heard above).
  fl = nflag ? sprintf(" flagged=%d flagged-family=%s", nflag, ffams) : ""
  if (why != "") {
    printf "<voice %.1fs>\n", ms / 1000
    printf "voice-unread:%s nt=%d m=%d maxrun=%d ms=%d%s%s\n", why, nt, mt, maxrun, ms, (why == "credit-leak" ? " family=" fam : ""), fl
    if (nleak) print heard
    exit
  }
  if (bad) printf "%s\n", text
  else printf "%s\t%s\n", text, pay
  if (nleak) printf "credit-dropped n=%d ms=%d family=%s%s\n%s\n", nleak, ms, fams, fl, heard
  else print "read"
}
# The piece from the lines of END, the credit lines dropped (drop 1) or not (0): the loop cut, the
# text and its timings (text, pay, bad) and why it is unread ("" for read), as assemble.rs piece() said.
function said(drop,   l, lf, p, k, g, s, np, zone, j) {
  delete sx; delete ncopy; delete lastp; delete after; delete zkept; delete gone
  k = 0; kj = ""
  for (l = 1; l <= nl; l++) {
    if (drop && (lf = lk[l]) != "" && (!real || lf != "silence-filler")) continue
    for (p = lj[l]; p < lq[l]; p++) {
      k++; uk[k] = ok[p]; ua[k] = oa[p]; ub[k] = ob[p]; ug[k] = og[p]
      kj = kj ok[p]; sx[og[p]] = sx[og[p]] ok[p]
    }
  }
  m = k
  # whisper loops on its own context at the tail of an utterance, segment by segment: one live line
  # (2026-11-25) ended in one 4-word segment 13 times and another 3 times, interleaved, at rigid
  # 1 s / 2 s spans. A loop text has LOOP_COPIES or more copies, the last of them among the
  # last LOOP_TAIL segments; its zone starts at its first copy whose next copy is at most
  # LOOP_SPACING segments on (the live copies were 1 to 5 apart), at the earliest such start
  # over all loop texts. From there each text keeps its first copy, later copies publish
  # neither text nor timings; before it nothing drops. Twice is real speech, so is a triple in
  # the middle of a line. A segment text: markers dropped, ends trimmed, compared exactly; a
  # segment with none is no copy and takes no position. The same rule as assemble.rs unlooped.
  LOOP_COPIES = 3; LOOP_TAIL = 2; LOOP_SPACING = 6
  np = 0
  for (g = 1; g <= ns; g++) {
    s = sx[g]; sub(/^[ \t]+/, "", s); sub(/[ \t]+$/, "", s)
    if (s == "") continue
    np++; pg[np] = g; ps[np] = s; ncopy[s]++; lastp[s] = np
  }
  for (p = np; p >= 1; p--) { s = ps[p]; nextp[p] = (s in after) ? after[s] : 0; after[s] = p }
  zone = 0
  for (p = 1; p <= np && !zone; p++) {
    s = ps[p]
    if (ncopy[s] >= LOOP_COPIES && lastp[s] + LOOP_TAIL > np && nextp[p] && nextp[p] - p <= LOOP_SPACING) zone = p
  }
  if (zone) for (p = zone; p <= np; p++) { s = ps[p]; if (s in zkept) gone[pg[p]] = 1; else zkept[s] = 1 }
  k = 0
  for (j = 1; j <= m; j++) {
    if (ug[j] in gone) continue
    k++; uk[k] = uk[j]; ua[k] = ua[j]; ub[k] = ub[j]
  }
  m = k
  # The turbo model emits subword pieces; a piece beginning with a space (or the first) begins
  # a word, the rest continue it — the span is the first piece t0 to the last piece t1, the
  # leading space trimmed, punctuation staying attached (glados, 2026-11-18).
  text = ""; pay = ""; bad = 0; wopen = 0
  for (j = 1; j <= m; j++) {
    if (j > 1 && ua[j] - ub[j-1] >= 500) {
      text = text "<pause " sprintf("%.1f", (ua[j] - ub[j-1]) / 1000) "s>"
      closeword()   # a word cannot span the gap the text itself names
    }
    if (!wopen || substr(uk[j], 1, 1) == " ") {
      closeword()
      wopen = 1; wt0 = ua[j]; ww = uk[j]
    } else {
      ww = ww uk[j]
    }
    wt1 = ub[j]
    text = text uk[j]
    if (index(uk[j], ";") > 0 || index(uk[j], "\t") > 0) bad = 1
  }
  closeword()
  sub(/^ +/, "", text); sub(/ +$/, "", text)
  why = ""
  if (drop && nleak && kj !~ /[^ ]/) why = "credit-leak"
  else if (text == "") why = nt == 0 ? "no-tokens" : (mt == 0 ? "markers-only" : "empty-text")
  else if (maxrun >= 6) why = "maxrun"
}
# The whisper model, given music, recites credits and calls to action out of its training
# data — fluently and with high token confidence (per-token p measured 0.79-0.94 on the
# junk against 0.891 on real speech, work-mac 2026-11-19: a confidence gate cannot tell
# them apart, it would eat real marginal speech instead). What does tell them apart is the
# closed set the model leaks. A line of the piece (the cut above) that begins with one of these
# is not speech in the room: it is dropped, and a piece of nothing else publishes as <voice N.Ns>,
# heard and unread, waking no one. The silence fillers match a whole line only, so a sentence
# that ends in one keeps it, and drop only with a piece of nothing else (the rule in END).
# This replaces the whole-line blacklist the shell loop used to carry (call logs,
# 2026-11-18): that one matched on SUBSTRINGS, so real speech merely mentioning subtitles
# was eaten whole and silent — the starts-with anchors here keep a spoken sentence about
# subtitles publishing as words.
# Every entry cites where it was seen; extend only with evidence. Every match is on norm() text,
# so case, punctuation and spacing never decide (#0454 B1 P7). A match returns the label of its
# family (the unread verdict names it; no text of the piece does), no match "".
function credit_leak(t,   f, n) {
  if ((f = leak_open(t)) != "") return f
  n = norm(t)
  if (n in FILL) return "silence-filler"   # whisper silence fillers, call logs: a whole line of just this
  if (index(n, SIGN) > 0) return "credit-name"   # the credit name never occurs in speech; any variant placement
  return ""
}
# The family of the start a line opens with (ls() in tables), "" for none.
function leak_open(t,   n, i) {
  n = norm(t)
  for (i = 1; i <= NLS; i++) if (index(n, LSN[i]) == 1) return LSF[i]
  return ""
}
# A credit own words in the line of tokens j..q-1 (#0454 B1 P6, assemble.rs own_words): the
# token after them, 0 for a line that opens with no start or is signed (cut to its credit
# already). They are the longest whole text (wc() in tables) the line opens with, word for word
# on norm() text; with none, the words its start covers (a start ending inside a word covers that
# word). An editor credit has its names after the start too (named), if they reach further. Words
# with no letter right after them are its punctuation, its own too.
function own_end(j, q,   p, s, all, st, sf, i, e, x, y) {
  s = ""
  for (p = j; p < q; p++) s = s uk[p]
  all = norm(s)
  if (index(all, SIGN)) return 0
  st = ""
  for (i = 1; i <= NLS; i++) if (index(all, LSN[i]) == 1) { st = LSN[i]; sf = LSF[i]; break }
  if (st == "") return 0
  e = 0; x = 0; s = ""
  for (p = j; p < q; p++) {
    s = s uk[p]
    if (p + 1 < q && substr(uk[p + 1], 1, 1) != " ") continue
    if (norm(s) in WHOLE) e = p + 1
    if (!x && index(norm(s), st) == 1) x = p + 1
  }
  if (!e) e = x
  if (sf == "subtitle-editor" && (y = named(x, q, j)) > e) e = y
  while (e < q) {
    s = uk[e]
    for (p = e + 1; p < q && substr(uk[p], 1, 1) != " "; p++) s = s uk[p]
    if (norm(s) != "") break
    e = p
  }
  return e
}
# Where the names of an editor credit (tokens j..q-1) end, its start ending at token p (#0454 B1
# D16, D19, D22, assemble.rs named): a name, then optionally Корректор and a name of its own. A word
# is judged on its case as written, never on norm() text; all-caps words count only in a credit
# whose start is all caps. Корректор is never a name. se: the line ends where its whisper segment
# does (the piece end or a segment bound), never at a stop or a gap (D20).
function named(p, q, j,   s, k, caps, se) {
  s = ""
  for (k = j; k < p; k++) s = s uk[k]
  caps = !index(shape(s), "l")
  se = q > m || ug[q] != ug[q-1]
  p = aname(p, q, caps, se)
  if (p < q && norm(wtext(p, q)) == PROOF) p = aname(wnext(p, q), q, caps, se)
  return p
}
# ONE name from the word at token p (default-to-real: a capitalised word before speech may be
# speech). A glued initial carries its surname and closes the slot; initials alone are always taken,
# and the capitalised word after them only where it closes; any other name is one capitalised word,
# taking a second only where that one closes.
function aname(p, q, caps, se,   k, n, t) {
  k = wkind(p, q, caps)
  if (!k) return p
  n = wnext(p, q)
  if (k == 2) return n
  if (k == 1) {
    while (wkind(n, q, caps) == 1) n = wnext(n, q)
    t = wkind(n, q, caps)
    return t == 2 || (t == 3 && closes(wnext(n, q), q, se)) ? wnext(n, q) : n
  }
  return wkind(n, q, caps) == 3 && closes(wnext(n, q), q, se) ? wnext(n, q) : n
}
# Whether a capitalised word ending before token p may still be a name: only Корректор or the
# segment end follows it.
function closes(p, q, se) { return p >= q ? se : norm(wtext(p, q)) == PROOF }
# The name kind of the word at token p, Корректор or none: 0.
function wkind(p, q, caps,   w) {
  if (p >= q || norm(w = wtext(p, q)) == PROOF) return 0
  return name_kind(w, caps)
}
# The token after the word that starts at token p, and that word, trimmed of spaces.
function wnext(p, q) { for (p++; p < q && substr(uk[p], 1, 1) != " "; p++) ; return p }
function wtext(p, q,   n, s) {
  n = wnext(p, q); s = ""
  for (; p < n; p++) s = s uk[p]
  sub(/^ +/, "", s); sub(/ +$/, "", s)
  return s
}
# A word as written shaped like a name (assemble.rs name_shape): 1 an initial, 2 an initial glued
# to a surname, 3 a capitalised word; in an all-caps credit the surname and the word are all caps of
# two letters or more; 0 for none. Punctuation after it is no part of its shape. Its shape: U for
# an uppercase letter and l for a lowercase one by the UP/LO table (no awk class or tolower() knows
# a Cyrillic letter under LC_ALL=C), the dot kept, anything else ?.
function name_kind(w, caps,   k) {
  sub(/^ +/, "", w); sub(/ +$/, "", w)
  if ((k = kind(shape(w), caps))) return k
  while (sub(/[.,!?]$/, "", w) || sub(/…$/, "", w)) ;
  return kind(shape(w), caps)
}
function kind(s, caps) {
  if (s == "U.") return 1
  if (caps ? s ~ /^U\.UU+$/ : s ~ /^U\.Ul+$/) return 2
  if (caps ? s ~ /^UU+$/ : s ~ /^Ul+$/) return 3
  return 0
}
function shape(t,   i) {
  for (i = 1; i <= NUP; i++) gsub(UP[i], "U", t)
  for (i = 1; i <= NUP; i++) gsub(LO[i], "l", t)
  gsub(/[^Ul.]/, "?", t)
  return t
}
# Text as the credit rules compare it (assemble.rs normal): letters kept, punctuation folded to a
# word break, each run of breaks one space, none at either end. A letter is an ASCII letter or
# digit, or any character outside ASCII but the NPX list; everything else is punctuation. Case
# folds by the UP/LO table (A-Z, А-Я, Ё): never tolower(), which neither the Mac awk nor mawk
# can be trusted to fold Cyrillic with. Written for an awk that counts bytes (length, substr, the
# per-character loop below): the awk on a Mac counts characters under a UTF-8 locale and judges
# differently, so the script runs under LC_ALL=C, set once at its top beside PATH.
function norm(t,   i, o, c) {
  for (i = 1; i <= NUP; i++) gsub(UP[i], LO[i], t)
  for (i = 1; i <= NNP; i++) gsub(NPX[i], " ", t)
  o = ""
  for (i = 1; i <= length(t); i++) { c = substr(t, i, 1); o = o (index(PUNCT, c) ? " " : c) }
  gsub(/  +/, " ", o); sub(/^ /, "", o); sub(/ $/, "", o)
  return o
}
# One start of a credit line and its family label.
function ls(f, s) { NLS++; LSF[NLS] = f; LSN[NLS] = norm(s) }
# The whole text of a credit as whisper recites it, word for word.
function wc(t) { WHOLE[norm(t)] = 1 }
function tables(   i, c, an) {
  NUP = split("A B C D E F G H I J K L M N O P Q R S T U V W X Y Z А Б В Г Д Е Ё Ж З И Й К Л М Н О П Р С Т У Ф Х Ц Ч Ш Щ Ъ Ы Ь Э Ю Я", UP, " ")
  split("a b c d e f g h i j k l m n o p q r s t u v w x y z а б в г д е ё ж з и й к л м н о п р с т у ф х ц ч ш щ ъ ы ь э ю я", LO, " ")
  NNP = split("… « » „ “ ” ‘ ’ – —", NPX, " "); NPX[++NNP] = "\302\240"   # and the no-break space
  an = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
  PUNCT = ""
  for (i = 1; i < 128; i++) { c = sprintf("%c", i); if (!index(an, c)) PUNCT = PUNCT c }
  ls("subtitles-created", "Субтитры созд")   # Субтитры создавал DimaTorzok: work-mac 2026-11-19, fixtures me-1795069373/478
  ls("subtitles-made", "Субтитры сдел")   # Субтитры сделал DimaTorzok: fixture me-1795069415
  ls("subtitle-editor", "Редактор субтитров")   # ...А.Семкин Корректор А.Егорова: fixtures me-1795069395/407/451
  ls("subscribe", "ПОДПИШИСЬ Н")   # ПОДПИШИСЬ НА КАНАЛ and its half-sentences: work-mac wakes 2026-11-19
  ls("dynamic-music", "ДИНАМИЧНАЯ МУЗЫКА")   # work-mac wakes 2026-11-19 morning, glados lane digest
  ls("thanks-for-viewing-ru", "Спасибо за просмотр")   # documented whisper leak family (thanks for watching)
  ls("enjoy-viewing-ru", "Приятного просмотра")   # documented whisper leak family
  ls("to-be-continued-ru", "Продолжение следует")   # documented leak family: the old line list, call logs
  ls("thank-you-for-watching", "Thank you for watching")   # documented leak family: the old line list
  ls("thanks-for-watching", "Thanks for watching")   # documented leak family: the old line list
  wc("Редактор субтитров А.Семкин Корректор А.Егорова")   # the three real captures, word for word
  wc("Редактор субтитров А.Семкин")   # #0454 B1 P8
  wc("ПОДПИШИСЬ НА КАНАЛ")
  wc("ПОДПИШИСЬ НА НАШ КАНАЛ")   # #0454 B1 P8
  wc("ДИНАМИЧНАЯ МУЗЫКА")
  wc("Спасибо за просмотр")
  wc("Приятного просмотра")
  wc("Продолжение следует")
  wc("Thank you for watching")
  wc("Thanks for watching")
  FILL[norm("Спасибо")] = 1; FILL[norm("Thank you")] = 1
  SIGN = norm("DimaTorzok"); SIGNBY = norm("Субтитр"); PROOF = norm("Корректор")
}
function closeword() {
  if (!wopen) return
  w = ww; sub(/^ +/, "", w)
  if (pay != "") pay = pay ";"
  pay = pay sprintf("%.0f", epoch + wt0) "," sprintf("%.0f", epoch + wt1) "," w
  wopen = 0
}
'
TAB=$(printf '\t')
NL='
'

# The raw ring (the owner, 2026-11-19): consumed pieces — each wav with its segment json — park
# in /tmp instead of leaving, byte-bounded, oldest dropped first. Policy in his words: LOCAL,
# overwritten, never shipped off the Mac unless he pulls it; nothing here auto-ships, and the
# ring lives in /tmp (EARS_RING_DIR overrides) — the system's own cleanup is the retention, his
# word: «пиши хвост в /tmp 0600 — он сам убьётся». The bound (EARS_RING_MAX_MB,
# default 8 MiB, validated and computed in preflight above) ≈ the last ~minute of both sides at
# 16 kHz mono 16-bit (32 kB/s ≈ 1.92 MB/min a side) plus slack; names carry the side so me/them
# never collide.
ring_keep() {  # ring_keep <side> <piece-name> — one consumed pair into the ring; oldest drops past the bound
  # F5 mid-run: the ring dir is re-checked on every use (builtins, no forks beyond one chmod) —
  # a symlink or foreign dir planted while we run drops the piece loudly instead of writing
  # through it. Preflight already refuses these before capture starts.
  ring_ready || { echo "ears-capture: ring unusable, dropping the piece ($1 $2)" >&2; rm -f "$D/$1/$2.wav" "$D/$1/$2.json" 2>/dev/null; return 0; }
  chmod 600 "$D/$1/$2.wav" 2>/dev/null
  # A failed move must not leave the piece behind: the loop would re-read it, re-transcribe it and
  # re-publish its line forever (akira, 2026-11-19, F3) — dropping it is the old behavior, loudly.
  # Mid-run states that make mv fail (disk full, ring deleted between checks) cannot be staged in
  # the rig since preflight hardened; this is the defense that stays.
  mv -f "$D/$1/$2.wav" "$RING/$1-$2.wav" 2>/dev/null \
    || { echo "ears-capture: ring move failed, dropping the piece ($1 $2)" >&2; rm -f "$D/$1/$2.wav"; }
  [ -f "$D/$1/$2.json" ] && { chmod 600 "$D/$1/$2.json"; mv -f "$D/$1/$2.json" "$RING/$1-$2.json" 2>/dev/null || rm -f "$D/$1/$2.json"; }
  # One process for the total (akira, 2026-11-19, F4): a wc fork per file per pass added ~160
  # forks to every published line; ls -ln's 5th column sums the same bytes. The trailing slash is
  # F5 belt-and-braces: it resolves a link, so the bound counts real files even if one got past.
  while :; do
    total=$(ls -ln "$RING"/ 2>/dev/null | awk '{s+=$5} END{print s+0}')
    [ "$total" -gt "$RING_MAX" ] || break
    old=$(ls -tr "$RING"/* 2>/dev/null | head -n 1)
    [ -n "$old" ] || break
    rm -f "$old" || break
  done
}

# One phrase this script heard and will not publish, told to the tray (stdout mode only) so its
# flow.log can say why: TULPA-EARS-DROP <why> [<side>] <text, possibly empty>. The tray never sends
# it over the link; it goes to the side log like every stdout line.
dropped() {  # dropped <why> <text>
  [ "${TULPA_LINES_STDOUT:-0}" = 1 ] || return 0
  printf 'TULPA-EARS-DROP %s [%s] %s\n' "$1" "$L" "$(printf '%s' "$2" | tr '\t\n\r' '   ')"
}

# One piece the assembler judged unread, or read with credit lines dropped, and its verdict (rule
# and inputs, never the piece's text): TULPA-EARS-DEBUG [<side>] voice-unread:<rule> nt=.. m=..
# maxrun=.. ms=.., or credit-dropped n=.. ms=.. family=.. — so a <voice N.Ns> can
# be read back after the fact (#0454 B1, 2026-11-25: a 57.5 s Zoom turn went <voice> and nothing
# said why). Stdout mode: the tray tees it to ears.log beside the line it explains and sends none
# of it over the link (it is no TULPA-EARS-LINE). File mode: stderr, the same bytes.
unread() {  # unread <verdict>
  if [ "${TULPA_LINES_STDOUT:-0}" = 1 ]; then
    printf 'TULPA-EARS-DEBUG [%s] %s\n' "$L" "$1"
  else
    printf 'TULPA-EARS-DEBUG [%s] %s\n' "$L" "$1" >&2
  fi
}

loop() { L=$1; OUT="$D/call-$L.txt"; W="$D/last-$L"
  hold=0; ht0=0; ht1=0; htx=""; hpay=""; hheard=""
  while :; do
    set -- $(ls "$D/$L"/c*.wav 2>/dev/null | sort)
    [ $# -ge $NEED ] || { sleep 0.3; continue; }
    f="$1"; j="${f%.wav}.json"; n="${f##*/}"; n="${n%.wav}"
    # The utterance's length (ms), how it ends, and its own span in the stream (t0/t1 ms), when
    # tulpa-segment cut it; else a 4 s window: no intonation, no span (0 0 — the tray falls back).
    ms=$(sed -n 's/.*"ms":\([0-9]*\).*/\1/p' "$j" 2>/dev/null); ms=${ms:-4000}
    t0=$(sed -n 's/.*"t0":\([0-9]*\).*/\1/p' "$j" 2>/dev/null); t0=${t0:-0}
    t1=$(sed -n 's/.*"t1":\([0-9]*\).*/\1/p' "$j" 2>/dev/null); t1=${t1:-0}
    into=$(sed -n 's/.*"intonation":"\([a-z]*\)".*/\1/p' "$j" 2>/dev/null)
    cut=$(sed -n 's/.*"cut":true.*/1/p' "$j" 2>/dev/null)
    who=""
    if [ "$L" = me ] && [ -n "$TAG" ] && [ -x "$TAG" ]; then
      who=$("$TAG" "$f" 2>/dev/null)
      case "$who" in self) ring_keep "$L" "$n"; dropped own-voice ""; continue;; esac   # our own voice, by the tagger's ear — raw audio still rings
    fi
    # -ojf: the timings live in the JSON (whisper.cpp prints none on stdout), -of names the file
    # (without it the JSON would land beside the wav). Stdout is noise here: the JSON is the text.
    rm -f "$W.json"
    set -- whisper-cli -m "$MODEL" --vad -vm "$VAD" -l "${EARS_LANG:-ru}" -np -f "$f" -ojf -of "$W"
    [ -n "${EARS_PROMPT-}" ] && set -- "$@" --prompt "$EARS_PROMPT"
    "$@" >/dev/null 2>&1
    ring_keep "$L" "$n"
    if [ ! -s "$W.json" ]; then
      echo "ears-capture: whisper-cli left no transcript for one utterance ($L)" >&2
      dropped no-transcript ""
      sleep 0.3
      continue
    fi
    tp=$(awk -v epoch="$t0" -v ms="$ms" "$ASSEMBLE" "$W.json"); rc=$?
    # The assembler's two lines: the piece, then its verdict ("read", voice-unread:… or
    # credit-dropped …; any but read goes to unread()), and after a dropped credit a third, the
    # text with no credit dropped, which the prompt-echo rule reads (#0454 B1 D7). No piece
    # at all (awk failed) is unread too — the empty line below makes it <voice> — and a piece with
    # no verdict line is a broken contract, said loudly as unknown rather than not at all. The
    # verdict comes only from a line that is one, by the words the assembler prints there (#0454
    # B1 D24): any other line in its place (heard or a piece out of order) may hold the piece's
    # text, which no record carries (P5), so it too is unknown.
    why=""; case "$tp" in *"$NL"*) why=${tp#*"$NL"}; tp=${tp%%"$NL"*};; esac
    heard=""; case "$why" in *"$NL"*) heard=${why#*"$NL"}; why=${why%%"$NL"*};; esac
    case "$why" in read|"voice-unread:"[a-z]*" nt="*|"credit-dropped n="*) ;; *) why="";; esac
    if [ -z "$tp" ]; then unread "voice-unread:assembler-failed rc=$rc ms=$ms"
    elif [ -z "$why" ]; then unread "voice-unread:unknown ms=$ms"
    elif [ "$why" != read ]; then unread "$why"
    fi
    t=${tp%%"$TAB"*}; [ "$t" = "$tp" ] && pay="" || pay=${tp#*"$TAB"}
    [ -n "$heard" ] || heard=$t
    # A max-length cut is not the end of the phrase (the owner: only the pause ends a turn): hold and
    # merge, so the line leaves at its close, at once.
    if [ "$hold" = 1 ]; then
      if [ $((t0 - ht1)) -ge 500 ]; then
        gap="<pause $(awk -v a="$t0" -v b="$ht1" 'BEGIN{printf "%.1f", (a-b)/1000}')s>"
        htx="$htx$gap"; hheard="$hheard$gap"
      else
        [ -n "$htx" ] && htx="$htx "   # pieces of one phrase with no pause between: a breath, not glue
        [ -n "$hheard" ] && hheard="$hheard "
      fi
    else
      ht0=$t0; htx=""; hpay=""; hheard=""
    fi
    ht1=$t1; htx="$htx$t"; hheard="$hheard$heard"
    if [ -n "$pay" ]; then [ -n "$hpay" ] && hpay="$hpay;"; hpay="$hpay$pay"; fi
    if [ "$cut" = 1 ]; then
      # Hold the turn open — but not past HOLDMAX of held speech: a never-pausing monologue
      # ships there, mid-phrase, and the speech after it becomes the next line. No span
      # (t0 0, a missing segment json): nothing to measure, hold as before.
      if [ "$ht0" = 0 ] || [ $((ht1 - ht0)) -lt "$HOLDMAX" ]; then hold=1; continue; fi
    fi
    hold=0
    # Subtitle-crew watermarks and whisper silence fillers are caught per piece in the
    # assembler (credit_leak above): matching here on merged-line substrings ate real
    # speech that merely mentioned subtitles. An empty phrase is not junk: it is
    # <voice N.Ns>, heard and unread, below.
    if [ -s "$D/prompt-pairs" ] && printf '%s\n' "$hheard" | grep -qFf "$D/prompt-pairs"; then
      dropped prompt-echo "$hheard"
      continue   # the prompt's own vocabulary echoed back on silence
    fi
    if [ -z "$htx" ]; then
      htx="<voice $(awk -v a="$ht1" -v b="$ht0" 'BEGIN{printf "%.1f", (a-b)/1000}')s>"   # heard, not read: still his signal
      hpay=""
    fi
    spk=""; [ -n "$who" ] && spk=" [${who%% *}]"
    tone=""; case "$into" in rising|falling) tone=" [intonation: $into]";; esac   # a rising end often marks a Russian question
    if [ "${TULPA_LINES_STDOUT:-0}" = 1 ]; then
      if [ -n "$hpay" ]; then
        printf 'TULPA-EARS-LINE %s %s %s [%s]%s %s%s%s%s\n' "$ht0" "$ht1" "$(date -u +%H:%M:%S)" "$L" "$spk" "$htx" "$tone" "$TAB" "$hpay"
      else
        printf 'TULPA-EARS-LINE %s %s %s [%s]%s %s%s\n' "$ht0" "$ht1" "$(date -u +%H:%M:%S)" "$L" "$spk" "$htx" "$tone"
      fi
    else
      printf '%s [%s]%s %s%s\n' "$(date -u +%H:%M:%S)" "$L" "$spk" "$htx" "$tone" | tee -a "$OUT"
    fi
    [ -n "$who" ] && printf '%s\tme\t%s\t%s\n' "$(date -u +%FT%TZ)" "$who" "$htx" >> "$D/me-speaker.tsv"
  done; }
loop them & A=$!
loop me & B=$!
# With CUT=pauses both sides are subshells (source | segment): pkill -P ends each pair.
# Stop the restart loop first (SIGSTOP: it can't spawn a new sysaudio mid-cleanup), kill its
# children, then it, then the mic pair and the loops.
trap 'kill -STOP $T 2>/dev/null; pkill -P $T 2>/dev/null; pkill -P $F 2>/dev/null; kill $T $F $A $B 2>/dev/null; kill -CONT $T 2>/dev/null; exit' INT TERM EXIT
echo "ears-capture: them=$SYSAUDIO (self-restarting), me=$MIC → $D (cut on $CUT), $( [ "${TULPA_LINES_STDOUT:-0}" = 1 ] && echo 'lines on stdout' || echo "transcript files in $D")"
wait $F
