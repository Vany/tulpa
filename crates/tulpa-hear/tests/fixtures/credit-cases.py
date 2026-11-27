#!/usr/bin/env python3
"""Writes credit-cases.txt, the cases script_parity's a_credit_costs_only_its_own_words runs both copies on.

Usage: credit-cases.py <base ears-capture.sh> > credit-cases.txt, the base being the script before credits were cut
per line (git show 67b806c7:packaging/macos/ears-capture.sh). Run once; the file is committed.

A case is a piece of whisper segments planted with credits. Its expected line and verdict come from the property
(#0454 B1, "a credit removes only its own words"), never from the copies under test:
- with a real word in the piece, the line is the BASE assembler's on the same piece with each credit's tokens removed
  by hand (a silence filler is speech beside a real word, so it stays), and the verdict is `credit-dropped n=<credits>
  ms=.. family=<each family once, in order>`, or `read` with no credit;
- with none (credits, fillers, empty and markers-only segments), `<voice>` and `voice-unread:credit-leak` with the
  piece's counts, naming the first credit's family.
A credit's OWN words (#0454 B1 P6) are the generator's knowledge of what it planted, never asked of either copy:
each FAMILIES entry says how many of its words are its own (a whole listed text: all; an open-ended start: the words
the start covers). The words after them, and speech glued after a credit in its line (same segment, no stop, no
pause), are kept as speech and the credit is FLAGGED: the verdict ends in `flagged=<n> flagged-family=<families>`.
A signed credit (DimaTorzok) is a line of its own and a filler a whole line, so neither is ever flagged.
P9, the spelling axis: every FAMILIES text (the fillers once each) x the punctuation after it (none . , ! …) or a
longer recital (`more`: one or two more words, drawn, no stop) x case (as listed, lower, title, upper) x inner spacing (single, double),
as a piece of its own and sharing one segment with the speech after it at gap 499 (`p9-` cases).
The axes: every family x position x neighbour x where the credit sits (its own segment; sharing one with the speech
before it, after it, or both) x with and without a sentence end x the gap after it. A share is generated where
the credit has speech on that side; with two credits, each shares where it can.

Format, one record per line: `case <name>`, `seg` (a new segment), `tok <from> <to> <text>` (the text to the line's
end, with its leading space), `line <expected piece line>`, `verdict <expected verdict line>`.
"""
import os
import random
import re
import subprocess
import sys
import tempfile

# The case file's timings are from the utterance's start (epoch 0): the property is about which words, and the
# epoch's arithmetic has its own tests.
EPOCH, MS = 0, 3400
# Each credit family whisper recites (the script's labels), with a line it recites for it and how many of its
# words are the credit's own (None: all of them).
FAMILIES = [
    ("subtitles-created", "Субтитры создавал DimaTorzok"),
    ("subtitles-made", "Субтитры сделал DimaTorzok"),
    ("subtitle-editor", "Редактор субтитров А.Семкин Корректор А.Егорова"),
    ("subtitle-editor", "Редактор субтитров А.Семкин"),
    ("subtitle-editor", "Редактор субтитров И.Петрова"),
    ("subscribe", "ПОДПИШИСЬ НА КАНАЛ"),
    ("subscribe", "ПОДПИШИСЬ НА НАШ КАНАЛ"),
    ("dynamic-music", "ДИНАМИЧНАЯ МУЗЫКА"),
    ("thanks-for-viewing-ru", "Спасибо за просмотр!"),
    ("enjoy-viewing-ru", "Приятного просмотра!"),
    ("to-be-continued-ru", "Продолжение следует..."),
    ("thank-you-for-watching", "Thank you for watching!"),
    ("thanks-for-watching", "Thanks for watching!"),
    ("silence-filler", "Спасибо."),
    ("silence-filler", "Спасибо!"),
    ("silence-filler", "Thank you."),
    ("credit-name", "DimaTorzok"),
]
WORDS = [" да", " нет", " мы", " завтра", " встреча", " в", " офисе,", " ok", " ну", " так.", " Привет", " what?"]
PIECES = [(" При", "вет"), (" сего", "дня"), (" ка", "нал"), (" sub", "titles")]
MENTIONS = [" Субтитры", " субтитры", " watching"]
MARKERS = ["[_BEG_]", "[_TT_42]", "[_TT_1500]", "[_EOT_]"]
STOPS = (".", "!", "?", "…")
# Either side of the pause that ends a line (500 ms); 0 and 600 lie on the same sides as these.
GAPS = (499, 500)
# D16: the subtitle editor's credit is its start, a name, and optionally Корректор with a name of its own; or its
# longest listed whole text, if that is longer. Names are judged on the case as written (all-caps ones only in an
# all-caps credit). The slot holds ONE name (D22): a glued initial carries its surname and closes it; initials alone
# ("И.", "А. Б.") are always taken, and the capitalised word after them is the surname only when nothing but Корректор
# or the SEGMENT end follows it (D19, D20); any other name is one capitalised word, and takes a second only on the same
# condition (D16). Корректор is never a name. The end of the text own_of() is given stands for the segment end: every
# FAMILIES name is glued, so a planted line's own words never hinge on what follows them.
EDITOR = ("редактор", "субтитров")
EDITOR_WHOLE = ("редактор субтитров а.семкин корректор а.егорова", "редактор субтитров а.семкин")
INITIAL = re.compile(r"[A-ZА-ЯЁ]\.")
# (glued initial and surname, capitalised word), as written and in an all-caps credit
NAME = (re.compile(r"[A-ZА-ЯЁ]\.[A-ZА-ЯЁ][a-zа-яё]+"), re.compile(r"[A-ZА-ЯЁ][a-zа-яё]+"))
CAPS_NAME = (re.compile(r"[A-ZА-ЯЁ]\.[A-ZА-ЯЁ]{2,}"), re.compile(r"[A-ZА-ЯЁ]{2,}"))
PUNCT_AFTER = ".,!?…"


def own_of(text):
    """How many of a planted text's words are the credit's own (None: all), whatever its punctuation."""
    words = text.split()
    bare = [w.rstrip(PUNCT_AFTER) for w in words]
    if tuple(w.lower() for w in bare[:2]) != EDITOR:
        return None
    whole = max((k for k in range(len(words) + 1) if " ".join(bare[:k]).lower() in EDITOR_WHOLE), default=0)
    glued, word = CAPS_NAME if not any(c.islower() for c in "".join(words[:2])) else NAME

    def proof(k):
        return k < len(words) and bare[k].lower() == "корректор"

    def shape(k):
        if k >= len(words) or proof(k):
            return None
        for kind, rx in (("initial", INITIAL), ("glued", glued), ("word", word)):
            if rx.fullmatch(words[k]) or rx.fullmatch(bare[k]):
                return kind
        return None

    def closes(k):
        return k == len(words) or proof(k)

    def name(k):
        kind = shape(k)
        if kind is None:
            return k
        if kind == "glued":
            return k + 1
        if kind == "initial":
            while shape(k) == "initial":
                k += 1
            last = shape(k)
            return k + 1 if last == "glued" or (last == "word" and closes(k + 1)) else k
        return k + 2 if shape(k + 1) == "word" and closes(k + 2) else k + 1

    k = name(2)
    if proof(k):
        k = name(k + 1)
    own = max(whole, k)
    return None if own >= len(words) else own
SIGNED = ("subtitles-created", "subtitles-made", "credit-name")
# Words a longer recital goes on in (P9): plain speech, no stop, opening no credit.
EXTRA = [" завтра", " встреча", " ok"]


def glued_own(text, speech):
    """D16: how many of the speech tokens glued after a credit in its line (same segment, no stop, no pause) the
    credit's own words take, by the rule on the whole line: an editor's name may take a capitalised word."""
    n = len(text.split())
    if tuple(w.rstrip(PUNCT_AFTER).lower() for w in text.split()[:2]) != EDITOR:
        return 0  # every other credit's own words are its listed text or its start's
    words = []  # per word of the speech: its token count
    for t in speech:
        if t.startswith(" ") or not words:
            words.append(0)
        words[-1] += 1
    own = own_of(text + "".join(speech))
    take = len(words) if own is None else max(0, own - n)
    return sum(words[:take])


def ojf(segs):
    s = '{\n\t"transcription": [\n'
    for k, toks in enumerate(segs):
        s += '\t\t{\n\t\t\t"tokens": [\n'
        for i, (t, a, b) in enumerate(toks):
            t = t.replace("\\", "\\\\").replace('"', '\\"')
            s += '\t\t\t\t{\n\t\t\t\t\t"text": "%s",\n\t\t\t\t\t"offsets": {\n\t\t\t\t\t\t"from": %d,\n\t\t\t\t\t\t"to": %d\n\t\t\t\t\t}\n\t\t\t\t}%s\n' % (
                t, a, b, "," if i + 1 < len(toks) else "")
        s += "\t\t\t]\n\t\t}%s\n" % ("," if k + 1 < len(segs) else "")
    return s + "\t]\n}\n"


def credit_tokens(r, text, own=None, sep=" "):
    """A credit line as whisper's tokens: each word with its leading space (`sep`), a long one sometimes in two
    pieces; each token with whether it is the credit's own (its first `own` words; None: every word)."""
    out = []
    for k, w in enumerate(text.split(" ")):
        mine = own is None or k < own
        if len(w) >= 4 and r.random() < 1 / 3:
            at = r.randint(1, len(w) - 1)
            out += [(sep + w[:at], mine), (w[at:], mine)]
        else:
            out.append((sep + w, mine))
    return out


def flags(fam):
    """Whether a planted credit's line can go on past its own words: never for a signed credit or a filler."""
    return fam not in SIGNED and fam != "silence-filler"


def speech_tokens(r):
    """Real-shaped speech: a word, whole or in subword pieces."""
    n = 1  # one word per slot keeps the file under 1 MB (#0454 B1 r2c); the mention needs three, so it is not drawn
    out, last = [], ""
    for k in range(n):
        if k == 0 and n == 3 and r.random() < 0.5:
            out.append(r.choice(MENTIONS))
        elif r.random() < 0.25:
            out += list(r.choice(PIECES))
        else:
            w = r.choice(WORDS)
            while w == last:
                w = r.choice(WORDS)
            last = w
            out.append(w)
    return out


def variant(text, stop):
    """The credit's line with a sentence end, or without one (None when that leaves no credit: a bare filler)."""
    bare = text.rstrip("".join(STOPS))
    if stop:
        return text if text.endswith(STOPS) else text + "."
    return None if bare != text and text in ("Спасибо.", "Спасибо!", "Thank you.") else bare


def pattern(pos, nb):
    """The slots: S speech, C the credit, X the neighbour (speech, another credit, an empty or a markers segment)."""
    x = [] if (pos == "only" and nb == "speech") else ["X"]
    return {
        "only": ["C"] + x,
        "first": ["C", "X", "S"],
        "middle": ["S", "C", "X", "S"],
        "last": ["S", "X", "C"],
        "two": ["S", "C", "X", "C", "S"],
    }[pos]


SHARES = ("own", "before", "after", "both")


def build(r, slots, nb, fam, other, text, other_text, share, gap):
    """Segments with the credits and without them, and the planted families in order; None when the share has no
    speech to share with."""
    kinds = [{"X": {"speech": "S", "credit": "O", "empty": "E", "markers": "M"}[nb]}.get(s, s) for s in slots]
    toks = []  # per slot: list of [text, kind]
    for k in kinds:
        if k == "S":
            toks.append([(t, "S") for t in speech_tokens(r)])
        elif k == "C":
            toks.append([(t, "C" if mine else "T") for t, mine in credit_tokens(r, text, own_of(text))])
        elif k == "O":
            toks.append([(t, "O" if mine else "T") for t, mine in credit_tokens(r, other_text, own_of(other_text))])
        elif k == "M":
            toks.append([(r.choice(MARKERS), "M")])
        else:
            toks.append([])
    # A shared credit joins the speech segment before it, after it, or both into one.
    segs_of = list(range(len(kinds)))
    shared = False
    for i, k in enumerate(kinds):
        if k != "C":
            continue
        before = i > 0 and kinds[i - 1] == "S"
        after = i + 1 < len(kinds) and kinds[i + 1] == "S"
        if share == "before" and before:
            segs_of[i] = segs_of[i - 1]
        elif share == "after" and after:
            segs_of[i + 1] = segs_of[i]
        elif share == "both" and before and after:
            segs_of[i] = segs_of[i + 1] = segs_of[i - 1]
        else:
            continue
        shared = True
    if share != "own" and not shared:
        return None
    for i, k in enumerate(kinds):
        if (k == "C" and 0 < i and kinds[i - 1] == "S" and segs_of[i] == segs_of[i - 1]) or k != "C":
            continue  # only a credit that opens its segment (one after speech in it is speech, D8')
        if i + 1 < len(kinds) and kinds[i + 1] == "S" and segs_of[i + 1] == segs_of[i] and gap < 500 \
                and not text.endswith(STOPS):
            took = glued_own(text, [t for t, _ in toks[i + 1]])
            toks[i + 1] = [(t, "C" if j < took else kind) for j, (t, kind) in enumerate(toks[i + 1])]
    at = 0
    timed = []  # (segment, text, from, to, kind)
    pending = 0
    for i, slot in enumerate(toks):
        for t, kind in slot:
            if kind == "M":
                timed.append((segs_of[i], t, at, at, kind))
                continue
            at += pending
            pending = 0
            timed.append((segs_of[i], t, at, at + 100, kind))
            at += 100
        if kinds[i] == "C":
            pending = gap
        if not slot and kinds[i] == "E":
            timed.append((segs_of[i], None, 0, 0, "E"))
    order = sorted(set(s for s, *_ in timed), key=lambda s: min(j for j, x in enumerate(timed) if x[0] == s))

    def segs(keep):
        out = []
        for s in order:
            seg = [(t, a, b) for (g, t, a, b, k) in timed if g == s and t is not None and keep(k)]
            empty_slot = any(g == s and k == "E" for (g, _, _, _, k) in timed)
            if seg or empty_slot:
                out.append(seg)
        return out

    planted = [(fam if k == "C" else other) for k in kinds if k in ("C", "O")]
    filler = {"C": fam == "silence-filler", "O": other == "silence-filler"}
    # D8': a credit is recognised only where a whisper segment starts; after speech in its segment (a stop or none) it
    # is speech, kept as said. A signed credit is a line of its own wherever it sits (D12: the signature is a
    # hallucination token; base drops the whole piece on the name anywhere).
    mid = {"C": False, "O": False}
    for i, k in enumerate(kinds):
        if k == "C" and fam not in SIGNED and i > 0 and kinds[i - 1] == "S" and segs_of[i] == segs_of[i - 1]:
            mid["C"] = True
    with_ = segs(lambda k: True)
    without = segs(lambda k: k not in ("C", "O") or filler[k] or mid[k])
    words = [t for (_, t, _, _, k) in timed if k in ("S", "T")]
    credits = [p for p, k in zip(planted, [k for k in kinds if k in ("C", "O")]) if not filler[k] and not mid[k]]
    # P6: a credit is flagged when its line goes on past its own words: its own tail words, or speech glued after it
    # in its segment with no stop and no pause.
    flagged = []
    for i, k in enumerate(kinds):
        if k not in ("C", "O"):
            continue
        f, t = (fam, text) if k == "C" else (other, other_text)
        if not flags(f) or mid[k]:
            continue
        tail = any(kind == "T" for _, kind in toks[i])
        glued = (i + 1 < len(kinds) and kinds[i + 1] == "S" and segs_of[i + 1] == segs_of[i] and gap < 500
                 and not t.endswith(STOPS) and any(kind == "S" for _, kind in toks[i + 1]))
        if tail or glued:
            flagged.append(f)
    return with_, without, planted, credits, bool(words), flagged


def base_out(awk, segs, d):
    f = os.path.join(d, "p.json")
    with open(f, "w") as fh:
        fh.write(ojf(segs))
    out = subprocess.run(["awk", "-v", "epoch=%d" % EPOCH, "-v", "ms=%d" % MS, awk, f], capture_output=True,
                         env={"LC_ALL": "C", "PATH": "/usr/bin:/bin"}, check=True).stdout.decode()
    return out.rstrip("\n").split("\n")


def counts(segs):
    toks = [t for s in segs for (t, _, _) in s]
    text = [t for t in toks if not (t.startswith("[_") and t.endswith("]"))]
    run = best = 0
    prev = None
    for t in text:
        run = run + 1 if t == prev else 1
        prev = t
        best = max(best, run)
    return len(toks), len(text), best


def flag_fields(flagged):
    return " flagged=%d flagged-family=%s" % (len(flagged), ",".join(dict.fromkeys(flagged))) if flagged else ""


def expect(awk, d, with_, without, planted, credits, speech, flagged):
    """The expected line and verdict: the base on the piece with the credits' own words removed by hand, or <voice>."""
    if speech:
        piece, verdict = base_out(awk, without, d)
        if verdict != "read":
            sys.exit("base verdict %r" % verdict)
        verdict = ("credit-dropped n=%d ms=%d family=%s" % (len(credits), MS, ",".join(dict.fromkeys(credits)))
                   if credits else "read") + flag_fields(flagged)
    else:
        nt, m, mr = counts(with_)
        piece = "<voice %.1fs>" % (MS / 1000)
        verdict = "voice-unread:credit-leak nt=%d m=%d maxrun=%d ms=%d family=%s" % (nt, m, mr, MS, planted[0])
    return piece, verdict


def emit(name, with_, piece, verdict):
    print("case " + name)
    for seg in with_:
        print("seg")
        for (t, a, b) in seg:
            print("tok %d %d %s" % (a, b, t))
    print("line " + piece)
    print("verdict " + verdict)


CASES = {
    "as": lambda w: w,
    "lower": str.lower,
    "title": lambda w: " ".join(x[:1].upper() + x[1:].lower() for x in w.split(" ")),
    "upper": str.upper,
}
TAILS = ("none", ".", ",", "!", "…", "more")


def spelled(r, fam, text0):
    """P9: every spelling of one listed text, as (label, tokens with their kinds, own_stop) — the kinds C (own), T (kept
    tail: the open-ended start's words, or a recital's extra words; for a filler, any extra word makes it speech)."""
    base = text0.rstrip("".join(STOPS))
    out = []
    for tail in TAILS:
        seen = set()
        for case, f in CASES.items():
            for sep in (" ", "  "):
                text = f(base)
                if (text, sep) in seen or (sep == "  " and " " not in base):
                    continue  # the same spelling as one already made (e.g. upper of an all-caps text)
                seen.add((text, sep))
                toks = credit_tokens(r, text, own_of(text), sep)
                toks[0] = (toks[0][0][len(sep) - 1:], toks[0][1])  # the line's first word: one leading space
                if tail == "more":
                    toks += [(w, False) for w in EXTRA[:r.randint(1, 2)]]
                elif tail != "none":
                    last, mine = toks[-1]
                    toks[-1] = (last + tail, mine)
                kinds = [("C" if mine else "T") for _, mine in toks]
                if fam == "silence-filler" and tail == "more":
                    kinds = ["S"] * len(toks)  # a filler with more words is speech
                out.append(("%s-%s-%s" % ({".": "dot", ",": "comma", "!": "bang", "…": "dots"}.get(tail, tail),
                                           case, "double" if sep == "  " else "single"),
                            [(t, k) for (t, _), k in zip(toks, kinds)], tail in (".", "!", "…")))
    return out


def p9_cases(awk, d, r):
    """P9's spelling axis: the credit as a whole piece of its own, and sharing one segment with the speech after it."""
    n = 0
    seen = set()
    for f, (fam, text0) in enumerate(FAMILIES):
        if (fam, text0.rstrip("".join(STOPS))) in seen:
            continue
        seen.add((fam, text0.rstrip("".join(STOPS))))
        for label, toks, stop in spelled(r, fam, text0):
            for share in ("own", "after"):
                speech = speech_tokens(r) if share == "after" else []
                if fam == "silence-filler" and share == "after":
                    toks = [(t, "S") for t, _ in toks]  # a filler beside a real word is speech (P2)
                timed, at = [], 0
                for t, k in toks:
                    timed.append((0, t, at, at + 100, k))
                    at += 100
                at += 499
                took = glued_own("".join(t for t, _ in toks), speech) if not stop and fam != "silence-filler" else 0
                for j, t in enumerate(speech):
                    timed.append((0, t, at, at + 100, "C" if j < took else "S"))
                    at += 100
                with_ = [[(t, a, b) for (_, t, a, b, _) in timed]]
                without = [[(t, a, b) for (_, t, a, b, k) in timed if k != "C"]]
                credit = any(k == "C" for _, k in toks)
                tail = any(k == "T" for _, k in toks)
                flagged = [fam] if credit and flags(fam) and (tail or (share == "after" and not stop and took < len(speech))) else []
                kept = any(k != "C" for (_, _, _, _, k) in timed)
                piece, verdict = expect(awk, d, with_, without, [fam], [fam] if credit else [], kept, flagged)
                emit("p9-%d-%s-%s-%s" % (f, fam, label, share), with_, piece, verdict)
                n += 1
    return n


# D8': (the speech before the credit, whether it is a segment of its own, the gap before the credit).
OPENERS = {
    "segstart": ([" мы"], True, 0),  # the speech in a segment of its own: the credit opens the next
    "stop": ([" так."], False, 0),  # after a sentence end in the same segment
    "comma": ([" офисе,"], False, 0),  # after a comma: it folds in the match but opens no line
    "plain": ([" мы"], False, 0),  # after a plain word
    "gap": ([" мы"], False, 500),  # after a pause that ends a line, inside the segment
}
D8_PUNCT = ("", ".", ",", "…")  # "!" lies on "."'s side: a stop in ASCII (size)
# Size (1.0 MB): after a comma, a plain word or a gap no case is recognised, so two cases there (as listed, all caps)
# separate a rule that matches from one that doesn't; the segment start and the stop (r2d's rule) keep all four.
D8_CASES = {o: ("as", "upper") for o in ("comma", "plain", "gap")}


def d8_cases(awk, d):
    """D8': each family's credit (one text each, fillers included) at a segment start or inside a segment (after a
    stop, a comma, a plain word, a gap of 500 ms), x case x the punctuation after it, then a plain word in its segment.
    Segment start: the base on the piece with the credit's own words removed by hand (a filler stays: speech beside a
    real word). Inside a segment: the base on the unchanged piece, read."""
    n = 0
    seen = set()
    for f, (fam, text0) in enumerate(FAMILIES):
        bare = text0.rstrip("".join(STOPS))
        key = bare if fam == "silence-filler" else fam  # one text a family; both fillers' words
        if fam in SIGNED or key in seen:
            continue
        seen.add(key)
        for opener, (before, own_seg, pause) in OPENERS.items():
            opens = own_seg
            for case, fold in CASES.items():
                if case not in D8_CASES.get(opener, CASES):
                    continue
                for punct in D8_PUNCT:
                    words = fold(bare).split(" ")
                    words[-1] += punct
                    own = own_of(" ".join(words))
                    ctoks = [(" " + w, "C" if (own is None or k < own) else "T") for k, w in enumerate(words)]
                    # Each token with the gap before it: the pause falls before the credit's first word.
                    seg_b = [(t, "S", 0) for t in before]
                    seg_c = [(t, k, pause if i == 0 else 0) for i, (t, k) in enumerate(ctoks)] + [(" фильма", "S", 0)]
                    segs_k = [seg_b, seg_c] if own_seg else [seg_b + seg_c]
                    timed, at = [], 0
                    for g, seg in enumerate(segs_k):
                        for t, k, gap in seg:
                            at += gap
                            timed.append((g, t, at, at + 100, k))
                            at += 100
                    credit = opens and fam != "silence-filler"
                    drop = (lambda k: k == "C") if credit else (lambda k: False)
                    with_ = [[(t, a, b) for (g, t, a, b, _) in timed if g == x] for x in range(len(segs_k))]
                    without = [[(t, a, b) for (g, t, a, b, k) in timed if g == x and not drop(k)] for x in range(len(segs_k))]
                    stop = punct in STOPS
                    flagged = [fam] if credit and flags(fam) and (own is not None or not stop) else []
                    piece, verdict = expect(awk, d, with_, without, [fam], [fam] if credit else [], True, flagged)
                    emit("d8-%d-%s-%s-%s-%s" % (f, fam, opener, case, {"": "none", ".": "dot", ",": "comma", "…": "dots"}[punct]), with_, piece,
                         verdict)
                    n += 1
    return n


# Pinned by name, so a change to them is deliberate: (name, segments of (token, the family of the credit it is
# planted in, or None for speech), the families flagged). Each run of one family's tokens is one credit.
SC, SE = "subtitles-created", "subtitle-editor"
NAMED = [
    # D11: credit words after speech in one segment, no stop: speech, unflagged.
    ("d11-subscribe-after-speech", [[(" мы", None), (" обсудили", None), (" Подпишись", None), (" на", None),
                                     (" канал", None)]], []),
    # D8': a credit after a sentence end in one segment, then more speech: all of it published, as base does.
    ("d8x-subscribe-after-stop", [[(" Он", None), (" ушёл.", None), (" ПОДПИШИСЬ", None), (" НА", None),
                                   (" КАНАЛ", None), (" Мы", None), (" ждём.", None)]], []),
    # D12: a signed credit inside a segment is still a line of its own and dropped, one per signed family.
    ("d12-subtitles-created-mid", [[(" Он", None), (" сказал", None), (" Субтитры", SC), (" создавал", SC),
                                    (" DimaTorzok", SC), (" и", None), (" ушёл", None)]], []),
    ("d12-subtitles-made-mid", [[(" Он", None), (" сказал", None), (" Субтитры", "subtitles-made"),
                                 (" сделал", "subtitles-made"), (" DimaTorzok", "subtitles-made"), (" и", None),
                                 (" ушёл", None)]], []),
    ("d12-credit-name-mid", [[(" Он", None), (" сказал", None), (" DimaTorzok", "credit-name"), (" и", None),
                              (" ушёл", None)]], []),
    # D14: after a signature that opens a line, the cut stays: a credit opening there is a credit (both dropped).
    ("d14-editor-after-signature", [[(" Субтитры", SC), (" создавал", SC), (" DimaTorzok", SC), (" Редактор", SE),
                                     (" субтитров", SE), (" А", SE), (".", SE), ("Сем", SE), ("кин", SE)],
                                    [(" мы", None), (" ждём", None)]], []),
    # P6a: the editor's name slot, judged on the case as written: up to two name-shaped words, then Корректор and a
    # slot of its own. Invented names in each script and case; whole credits in a segment of their own.
    ("p6a-cyr-capitalised", [[(" Редактор", SE), (" субтитров", SE), (" Ирина", SE), (" Волкова", SE)],
                             [(" мы", None), (" ждём", None)]], []),
    ("p6a-lat-capitalised", [[(" Редактор", SE), (" субтитров", SE), (" John", SE), (" Smith", SE)],
                             [(" мы", None), (" ждём", None)]], []),
    ("p6a-cyr-initial-then-speech", [[(" Редактор", SE), (" субтитров", SE), (" Л", SE), (".", SE), ("Орлова", SE),
                                      (" завтра", None), (" встреча", None)]], [SE]),
    ("p6a-lat-initial-then-speech", [[(" Редактор", SE), (" субтитров", SE), (" J", SE), (".Smith", SE),
                                      (" мы", None), (" ждём", None)]], [SE]),
    ("p6a-cyr-caps", [[(" РЕДАКТОР", SE), (" СУБТИТРОВ", SE), (" И.ПЕТРОВА", SE)], [(" мы", None), (" ждём", None)]],
     []),
    ("p6a-lat-caps", [[(" РЕДАКТОР", SE), (" СУБТИТРОВ", SE), (" J.SMITH", SE)], [(" мы", None), (" ждём", None)]],
     []),
    # An all-caps word in a credit that is not all caps is no name: it reads, flagged.
    ("p6a-caps-word-in-mixed", [[(" Редактор", SE), (" субтитров", SE), (" ИВАНОВ", None), (" мы", None)]], [SE]),
    # A lowercase verb right after the start is speech: it publishes, flagged.
    ("p6a-verb-after-start", [[(" редактор", SE), (" субтитров", SE), (" сказал", None), (" бы", None),
                               (" завтра", None)]], [SE]),
    # D16: a second capitalised word is a name only at the line's end or before Корректор: here both read, flagged.
    ("p6a-three-names", [[(" Редактор", SE), (" субтитров", SE), (" Анна", SE), (" Мария", None), (" Петрова", None)]],
     [SE]),
    # The long form with invented names, and the short one: all dropped, unflagged.
    ("p6a-long", [[(" Редактор", SE), (" субтитров", SE), (" Л.Орлова", SE), (" Корректор", SE), (" Н", SE),
                   (".Белова", SE)], [(" мы", None), (" ждём", None)]], []),
    ("p6a-short", [[(" Редактор", SE), (" субтитров", SE), (" Л.Орлова", SE)], [(" мы", None), (" ждём", None)]], []),
    # D16 (motoko 01:23Z), each token's family marked by hand: a name is an initial-led name ending at its surname, or
    # one capitalised word taking a second only before Корректор or the line's end.
    ("d16a-two-words-whole", [[(" Редактор", SE), (" субтитров", SE), (" Алексей", SE), (" Семкин", SE)],
                              [(" мы", None), (" ждём", None)]], []),
    ("d16b-surname-then-speech", [[(" Редактор", SE), (" субтитров", SE), (" Семкин", SE), (" Мы", None),
                                   (" обсуждали", None), (" релиз", None)]], [SE]),
    ("d16c-two-words-then-speech", [[(" Редактор", SE), (" субтитров", SE), (" Алексей", SE), (" Семкин", None),
                                     (" Мы", None), (" обсуждали", None)]], [SE]),
    ("d16d-glued-then-speech", [[(" Редактор", SE), (" субтитров", SE), (" А", SE), (".", SE), ("Сем", SE),
                                 ("кин", SE), (" Привет", None)]], [SE]),
    ("d16e-long-glued-then-speech", [[(" Редактор", SE), (" субтитров", SE), (" А.Семкин", SE), (" Корректор", SE),
                                      (" А.Егорова", SE), (" Привет", None)]], [SE]),
    ("d16f-long-two-words-whole", [[(" Редактор", SE), (" субтитров", SE), (" Алексей", SE), (" Семкин", SE),
                                    (" Корректор", SE), (" Анна", SE), (" Егорова", SE)],
                                   [(" мы", None), (" ждём", None)]], []),
    # D17: an initial, a space and the surname are one name: the initial's dot ends no sentence there.
    ("d17g-initial-space-surname-whole", [[(" Редактор", SE), (" субтитров", SE), (" И", SE), (".", SE),
                                           (" Петрова", SE)], [(" мы", None), (" ждём", None)]], []),
    # D19 (motoko 02:07Z): an initial is always taken; a later capitalised word only before Корректор or the SEGMENT
    # end (D20: never a stop or a line end). Pin h CHANGED from D17: "Петрова" is followed by speech, so it reads.
    ("d17h-initial-space-surname-then-speech", [[(" Редактор", SE), (" субтитров", SE), (" И.", SE), (" Петрова", None),
                                                 (" Мы", None), (" ждём", None)]], [SE]),
    ("d19i-initial-then-speech", [[(" Редактор", SE), (" субтитров", SE), (" И", SE), (".", SE), (" Мы", None),
                                   (" обсуждали", None)]], [SE]),
    ("d19j-two-initials-surname-whole", [[(" Редактор", SE), (" субтитров", SE), (" А", SE), (".", SE), (" Б", SE),
                                          (".", SE), (" Семкин", SE)], [(" мы", None), (" ждём", None)]], []),
    ("d19k-two-initials-surname-then-speech", [[(" Редактор", SE), (" субтитров", SE), (" А.", SE), (" Б.", SE),
                                                (" Семкин", None), (" Мы", None), (" ждём", None)]], [SE]),
    ("d19l-initial-at-segment-end", [[(" Редактор", SE), (" субтитров", SE), (" И", SE), (".", SE)],
                                     [(" мы", None), (" ждём", None)]], []),
    ("d19m-two-words-stop-speech-one-segment", [[(" Редактор", SE), (" субтитров", SE), (" Алексей", SE),
                                                 (" Семкин.", None), (" Мы", None), (" ждём", None)]], [SE]),
    ("d19n-two-words-stop-then-next-segment", [[(" Редактор", SE), (" субтитров", SE), (" Алексей", SE),
                                                (" Семкин.", SE)], [(" Мы", None), (" ждём", None)]], []),
    # D22 (main 03:3xZ): the slot holds one name; a glued initial after spaced ones carries its surname and closes it.
    ("d22o-initial-then-glued-then-speech", [[(" Редактор", SE), (" субтитров", SE), (" А", SE), (".", SE), (" Б", SE),
                                              (".", SE), ("Сем", SE), ("кин", SE), (" Привет", None)]], [SE]),
    # D16: Корректор is never a name: right after the start it opens the proofreader's slot, whose name is taken.
    ("d16p-korrektor-right-after-start", [[(" Редактор", SE), (" субтитров", SE), (" Корректор", SE), (" Анна", SE),
                                           (" Егорова", SE)], [(" мы", None), (" ждём", None)]], []),
]


def named_cases(awk, d):
    """NAMED: the base on the piece with the planted credits' words removed by hand (none for speech)."""
    for name, segs, flagged in NAMED:
        timed, at = [], 0
        for seg in segs:
            timed.append([])
            for t, fam in seg:
                timed[-1].append((t, at, at + 100, fam))
                at += 100
        credits = []
        prev = None
        for seg in timed:
            for (_, _, _, fam) in seg:
                if fam and fam != prev:
                    credits.append(fam)
                prev = fam
        with_ = [[(t, a, b) for (t, a, b, _) in seg] for seg in timed]
        without = [[(t, a, b) for (t, a, b, fam) in seg if not fam] for seg in timed]
        piece, verdict = expect(awk, d, with_, without, credits[:1] or [None], credits, True, flagged)
        emit(name, with_, piece, verdict)
    return len(NAMED)


def main():
    script = open(sys.argv[1], encoding="utf-8").read()
    awk = script.split("\nASSEMBLE='\n", 1)[1].split("\n'\n", 1)[0]
    r = random.Random(0x454B1)
    d = tempfile.mkdtemp()
    n = 0
    for f, (fam, text0) in enumerate(FAMILIES):
        ofam, otext = FAMILIES[(f + 1) % len(FAMILIES)]
        for pos in ("only", "first", "middle", "last", "two"):
            for nb in ("speech", "credit", "empty", "markers"):
                for share in (SHARES if pos != "two" else ("own",)):  # two credits: own segments only (size)
                    for stop in (True, False):
                        text = variant(text0, stop)
                        if text is None:
                            continue
                        # In its own segment the credit's line ends at the bound whatever the gap: one gap (size, D8).
                        for gap in (GAPS if share != "own" else GAPS[:1]):
                            built = build(r, pattern(pos, nb), nb, fam, ofam, text, otext, share, gap)
                            if built is None:
                                continue
                            piece, verdict = expect(awk, d, *built)
                            name = "%d-%s-%s-%s-%s-%s-gap%d" % (f, fam, pos, nb, share, "stop" if stop else "nostop",
                                                                 gap)
                            emit(name, built[0], piece, verdict)
                            n += 1
    p9 = p9_cases(awk, d, r)
    d8 = d8_cases(awk, d)
    named = named_cases(awk, d)
    os.remove(os.path.join(d, "p.json"))
    os.rmdir(d)
    print("cases %d, p9 %d, d8 %d, named %d" % (n, p9, d8, named), file=sys.stderr)


main()
