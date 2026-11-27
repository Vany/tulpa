# Using Tulpa

Tulpa is a menu-bar app on a Mac. It keeps one outbound SSH link to the hub, the server end of the link where the agent lives, and answers the agent's requests over it. The Mac listens on nothing. Over the link the agent has:

- **ears**: a capture of the microphone (side `me`) and the system audio (side `them`), a self-check of both sides, and transcript lines sent as events;
- **voice**: `voice.say`, spoken on the Mac;
- **hands**: files, git and named commands inside the roots the Mac's owner allows; in full mode, any shell command and any file. Big files travel on a data stream beside the link;
- **eyes**: `eyes.photo`, one still from the Mac's camera when the agent asks for one.

There is no browser organ and no browser verb. In full mode, anything else is a `shell` command.

The person at the Mac sees each organ's state in the menu and can pause or stop it there. This page is for daily work. Installing, updating and keeping a Mac healthy are in [DEVELOP.md](DEVELOP.md).

**Status.** This page describes the code in the commit that added it. There are no tagged releases yet. A feature that exists only on another branch is marked **not merged**; one that the hub side has built but not deployed is marked **planned**.

## Requests and replies

The agent uses the hub client. It names a device, a verb and, for some verbs, an argument, and prints what comes back. The hub sends each device one request at a time. Tulpa answers every request line with one reply line:

```text
→ {"id":"7","verb":"ears.mode","arg":"alone"}
← {"id":"7","ok":true,"result":"started (pid 4242)"}
→ {"id":"8","verb":"voice.say","arg":"The build is green."}
← {"id":"8","ok":false,"err":"paused"}
```

The hub client prints `result` (text line by line, an object as JSON) or the error. The errors you will meet:

| Error | From | Meaning |
| --- | --- | --- |
| `verb` | Tulpa or the hub | Unknown verb. While the hands are off, every hands verb, `ping` included, answers `verb` too. |
| `arg` | Tulpa or the hub | The argument is missing, empty or malformed. |
| `paused` | Tulpa | **Pause all** is on in the menu. |
| `offline` | the hub | The device has no link now. |
| `busy: <verb> #<n> in flight` | the hub | The device is still answering an earlier request. A long `voice.say` or `run` holds it. |
| `timeout` | the hub | No reply in time. The hub drops the link, and Tulpa dials again by itself. |

The hub in use takes one request of at most 64 KiB as sent (JSON-escaped). That bounds `write`.

The hub also grants verbs per caller. `ping`, `read`, `list`, `git.*` and `ears.status` are open to every caller. Every other verb needs a grant, or the hub refuses it before it reaches the Mac.

The hub client also lists the devices the hub knows: whether each is online and since when, its ears mode, the request in flight, and its last event.

## Verbs

### Ears and voice

These verbs need `[ears] mode = "on"` in the Mac's config. Otherwise they answer `ears: off in config`.

| Verb | Argument | Example | What comes back |
| --- | --- | --- | --- |
| `ears.start` | none | `ears.start` | `started (pid 4242)`: the `call` mode started. With a capture already running: `already running (pid 4242)`, and its mode stays. |
| `ears.stop` | none | `ears.stop` | `stopped`, or `not running`. The ears also start off after a restart. |
| `ears.mode` | `off`, `alone`, `office`, `meeting`, `call` or `stream` | `ears.mode alone` | `started (pid 4242)`, `already alone`, or `off`. Refused with `paused`, or with `mode stream is not configured ([ears.modes].stream)`. |
| `ears.status` | none | `ears.status` | An object, for example `{"state":"running","pid":4242,"since":"2026-01-15T09:30:00Z","mode":"alone","keepawake":"on (always, ears, -is)"}`. |
| `ears.test` | a short text | `ears.test hello` | `sent`. One transcript line goes out as an event: side `me`, the current mode, `"test":true`. It goes whatever the mode, even while paused. |
| `voice.say` | text, up to 500 characters | `voice.say The build is green.` | `said`, once the speech has played. Refused with `paused`. |

`ears.status` fields:

- `state`: `off`, `running`, `degraded`, `deaf` or `exited` (see [DEGRADED and DEAF](#degraded-and-deaf));
- `mode`: the running capture's mode, `off` when none runs;
- `pid` and `since` (UTC) while a capture runs;
- `side`, `reason` and `fix` while degraded; `reason` while deaf;
- `code` after the capture exited by itself (`null` when a signal ended it);
- `keepawake`: Tulpa's keep-awake, the same text as the menu's [keep-awake line](#the-menu).

`voice.say` details:

- Without an own voice, `/usr/bin/say -v <voice>` speaks the whole text, with `[ears] voice`.
- An own voice is a voice of the agent's, such as a cloned one, made by a command of yours. `[ears] voice_synth` runs once per sentence with the sentence as one argument and prints the path of a WAV; it gets 20 s. `[ears] voice_serve` keeps the model loaded: one resident process reads a sentence per line and answers with a WAV path; its first answer gets 90 s. `[ears] voice_post` may process each WAV. Tulpa deletes each WAV once it has played.
- With an own voice, the text is spoken sentence by sentence, and the next sentence renders while one plays. When the own voice fails, macOS `say` speaks that sentence instead, and the log says so.
- With `voice_serve_plays = true`, the resident gets the whole text and plays it itself. If it fails before any sound, Tulpa speaks sentence by sentence instead; after a sound, the error is the reply.
- **Voice output** in the menu (`[voice] output_device`) picks the output for `say` and for a resident that plays itself. A synthesized WAV plays with `afplay`, which follows the system default output.
- `[voice] rate`, from 0.5 to 2, sets the speed. It is applied once, by what plays the sound: `say` speaks at 175 × rate words a minute, `afplay` plays each WAV at that rate, and a resident that plays itself gets `--speed <rate>` unless its command has one. A tempo change in `voice_post` multiplies with it, so use one or the other. A rate outside 0.5 to 2 makes the `[ears]` section unusable: every ears and voice verb answers with that error.
- With `[ears] speaking_file` set, each playback window goes into that file as one `<start_ms> <end_ms>` line (`0` as the end while it plays, kept for 2 min), so the capture's transcriber can drop what the microphone heard of the agent's own voice.
- It waits up to 60 s while another clip plays (`afplay`).
- `[ears] mode` gates it. `[voice] enabled` does not.

### Hands

| Verb | Argument | Allowed | Example | What comes back |
| --- | --- | --- | --- | --- |
| `ping` | none | whenever the hands are on, even while paused | `ping` | `pong` |
| `read` | an absolute or `~/` path | inside a root and not denied; full mode: any path | `read ~/src/example/README.md` | The file's text. A binary file is refused. |
| `list` | a directory | as `read` | `list ~/src/example` | One line per entry: `d src`, `f README.md`, `l link`; denied names are left out. Full mode: names, with `/` after a directory. |
| `git.status` | a repository root | inside a root | `git.status ~/src/example` | `git status --porcelain=v1 --branch` |
| `git.diff` | a repository root | inside a root | `git.diff ~/src/example` | The unstaged changes; denied paths are left out. |
| `git.log` | a repository root | inside a root | `git.log ~/src/example` | The last 50 commits, one per line: hash, date, subject. |
| `run` | a command name | the policy names it | `run example-test` | `exit 0`, the output, then `--- stderr ---` and the errors if any. |
| `shell` | a command line | full mode only | `shell sw_vers -productVersion` | `exit 0` and the output, as `run`. |
| `write` | a path, a newline, the content | full mode only | `write ~/notes.txt` (the hub client takes the content from a file) | `wrote 12 bytes to /Users/<user>/notes.txt` |

More rules:

- A hands reply is cut at 256 KB and then ends with `[cut at 256 KB]`. Secret shapes (private keys, API tokens) become `[secret]` before it leaves the Mac.
- `git.*` needs the root of a repository with a real `.git` directory; worktrees, links, and a repository whose config defines filter drivers are refused.
- `run` starts a fixed argv in a fixed directory with a cleared environment. Its time limit comes from the policy: 120 s by default, at most 3600 s. `shell` runs `/bin/sh -c` as the user, from the home directory, for at most an hour.
- Pause kills a running command's whole process group (`paused (killed)`). So does a dropped link (`link down (killed)`) and the time limit (`timeout (killed)`).
- `write` creates missing directories and replaces the file whole. Through the hub the content must fit in one request.
- The policy allows command names of `[A-Za-z0-9_.-]`, up to 64. The hub in use passes only `[a-z0-9_-]` for `run`, so use lowercase names.

#### The data stream: `xfer.put` and `xfer.get`

A file never travels on the request line. The hub asks on the link and gets an answer at once. The bytes then travel on a second connection that the Mac dials itself, on the link's own route (a second `ssh -W`), so the Mac still listens on nothing. The hub's side is **planned**: built, not deployed.

| Verb | Argument, from the hub | Allowed | What comes back at once |
| --- | --- | --- | --- |
| `xfer.put` | `{ticket, path, size, sha256}` | the policy's `put` posture (default `ask`), into a root; full mode: any path | `{"offset":n}`: how much of the file the Mac already holds, where the bytes resume |
| `xfer.get` | `{ticket, path, offset}` | as `read`; full mode: any path; and the eyes' photos ([Eyes](#eyes)) | `{"size":n,"sha256":"…"}` of the file, before any byte |

- The ticket is the hub's, for one transfer. Tulpa says it once, on the data connection, and never logs it.
- **put**: the bytes go to `<path>.<first 12 hex digits of the sha256>.part` beside the target, opened without following a symlink. Exactly `size` bytes are taken, synced, checked against the sha256 and renamed onto the path. A replaced file keeps its permission bits, but never its set-user-ID or set-group-ID bit. The verdict, `{"ok":true,"size":n,"sha256":"…"}` or `{"ok":false,"err":"…"}`, goes back on the data connection. A cut transfer keeps its `.part`, and the next put of the same file resumes from it.
- **get**: the bytes start at `offset`, the size of the receiver's own `.part`. Before the last bytes leave, Tulpa checks the file's size and modification time again: a file that changed while it was sent fails short.
- One transfer at a time: `busy: a transfer is running`. Files up to 4 GiB; a put needs free space for the rest of the file plus 64 MiB.
- 60 s without a byte closes the data connection. Pause closes it at once.
- A `.part` that Tulpa made and nobody touched for 24 h is deleted when the next transfer starts. Tulpa marks each `.part` it makes, so it never deletes a file of yours that only looks like one.
- The hands line shows the transfer with the Mac's byte count, then its outcome. The activity log gets the path and the size, never the ticket.
- Without a configured link there is no data stream: `xfer: no data link`.

#### The files channel: `files.put` and `files.state`

The one always-on channel: files arrive whatever the hands' mode, whatever their policy, and while anything is paused — an upgrade bundle must not wait on the senses' state. It is the link's channel, not the hands': there is no posture and no click, and the server's key line is the other half of that bargain. It uses the data stream's own machinery — ticket, offset resume, sha256, the atomic rename — on **its own** data stream, so a hundred-megabyte bundle never queues behind a hands transfer or a photo fetch.

| Verb | Argument, from the hub | Allowed | What comes back at once |
| --- | --- | --- | --- |
| `files.put` | `{ticket, path, size, sha256}` | one name of `[A-Za-z0-9._+-]` (up to 128, not starting with a dot) into the inbox — no path, no posture, no click, no pause | `{"offset":n}`: the `.part`'s size, where the bytes resume, as `xfer.put` |
| `files.state` | – | always | `{"inbox":[…],"staging":[…]}`: the names the two directories hold |

- The inbox is one directory Tulpa owns: `~/Library/Application Support/tulpa/inbox`. The request's `path` is only the final name inside it; anything else — `..`, a `/`, a leading dot, a space — is refused before anything dials. The Mac's side never writes anywhere else; the hub's side of that bargain is the server's authorized-key line.
- The transfer behaves as `xfer.put` (`.part` beside the target, resume across a dropped link and a Tulpa restart, one at a time on this channel, up to 4 GiB, verdict on the data connection). Pause does not touch it: the bytes ride the link or wait for it.
- What lands is delivered at once. A bundle archive (`*.tgz`, `*.tar.gz`) holding exactly one `Tulpa.app` with a real executable and a `commit=` marker is unpacked, verified, and staged as `Tulpa.app-<commit>` in `~/Library/Application Support/tulpa/staging` — the name and directory the login build check (`build-check.sh`) installs from, hourly. The inbox copy is removed only once staged; anything else stays in the inbox for whoever asked for it.
- Each landed file is one line in the activity log and one event on the link: `{"event":"files","state":"staged"\|"kept"\|"failed","name":…}`. At Tulpa's start the inbox is taken again — a failed delivery retries, a cut unpack's debris is cleared — so after a restart the hub only needs its usual re-request and the `.part` answers with its offset.

### Port forwards

A forward joins a loopback port on the Mac to the same port on the server's loopback for a few minutes, through the link's own `ssh`. It is for a sign-in that needs `localhost`: an OAuth redirect that the Mac's browser follows to a tool listening on the server, or the reverse.

| Verb | Argument | What it does |
| --- | --- | --- |
| `forward.open` | `"<port> out [ttl=<s>] [once\|many]"` | **N →**: Tulpa listens on the Mac's `127.0.0.1:N`. Each connection there goes over its own `ssh -W 127.0.0.1:N` to the server. By default it closes after its first connection has ended (`once`), 5 min at most (`ttl`, up to 15 min). |
| `forward.open` | `"<port> in [ttl=<s>]"` | **N ←**: Tulpa runs `ssh -N -R 127.0.0.1:N:127.0.0.1:N` for the forward's life: the server's `127.0.0.1:N` reaches the Mac's. 15 min by default, up to 60. |
| `forward.close` | `"<port> <out\|in>"` | Closes it now. |
| `forward.list` | none | One line per open forward, `53682 → (4:12 left, once, 1 conn)`, or `none`. |

- **Only the Mac's own ports.** A port must be listed in `[forward] ports` in the Mac's config, whatever the hub granted. No ports listed: `forward: off on this Mac`.
- **Loopback only, only while open, always visible.** The Mac listens only for an open out forward, only on `127.0.0.1`, and the menu shows it. While it is open, any program or web page on the Mac can reach that port on the server. That is why out forwards close after one connection and last minutes.
- One forward per port and direction (`busy: 53682 → is open`). Pause all and Quit close every forward; resume opens none.
- An in forward the server refuses ends at once: `53682 ←: the server refused it (…)`, with ssh's own words.
- Every forward's `ssh` runs under a watchdog that ends it within 2 s once Tulpa is gone, even after a crash or a kill. Its pid is kept beside the log, and the next start ends any that is still running.
- The activity log gets each open, connection and close, with byte counts. It never gets a byte that was carried.
- The server must allow forwards for the Mac's key: [Port forwards on the server](DEVELOP.md#port-forwards-on-the-server).

### Eyes

`eyes.photo` takes one still with the Mac's default camera, the built-in one on a laptop, when the agent asks for one. Nothing streams and nothing repeats.

| Verb | Argument | Example | What comes back |
| --- | --- | --- | --- |
| `eyes.photo` | none | `eyes.photo` | `{"path":"/Users/<user>/Library/Application Support/tulpa/photos/photo-20260115T093000Z.jpg","size":183422,"sha256":"…"}`: where the JPEG is on the Mac, its size in bytes and its sha256 |

`[eyes] mode` in the Mac's config decides:

| `[eyes] mode` | What `eyes.photo` does |
| --- | --- |
| `off`, the default | answers `eyes: off in config` |
| `ask` | asks `take one photo with the camera` in [the Allow/Deny dialog](#the-allowdeny-dialog). **Deny**: `denied by the owner`; no click in 30 s: `no click in time` |
| `on` | takes the photo at once |

- The camera is Tulpa's own. A helper inside the app bundle takes the photo as Tulpa's child, so the macOS camera prompt names Tulpa and the grant is Tulpa's. The first photo waits for the person at the Mac to click **Allow** in that prompt (see [the camera grant](DEVELOP.md#the-camera-grant)). The camera light is macOS's: it is on while the helper runs, about 2 s.
- Refused with `paused` while **Pause all** is on. Pause during a photo kills the helper (`paused (killed)`), and so does a dropped link (`link down (killed)`).
- One photo at a time: `busy: a photo is being taken`. The helper gets 60 s, the first macOS prompt included: `camera: no photo in 60 s`, with a note when that prompt was not answered. Past the 60 s the helper is killed with its whole process group.
- The camera's own refusals come back as `camera: …`, such as `camera: denied (System Settings > Privacy & Security > Camera: allow Tulpa)` or `camera: no camera`.
- The photo is a `0600` file in `~/Library/Application Support/tulpa/photos`, a `0700` directory. Its name is the UTC second it was taken; another photo in the same second gets `-2`, `-3` and so on.
- The activity log gets one line per request, with the photo's name and size.

**How the agent gets the file:** with the data stream's `xfer.get`, the `path` from the reply and `offset` 0. While the eyes are on, the photo directory is the data stream's shelf:

- **The photo just answered** is the eyes' to hand over. The next `xfer.get` naming it, within 60 s of the reply, is sent whatever the hands' state: with `[hands] mode = "off"`, with no usable policy, and whatever their `read` posture. Taking the photo was the decision. **Pause all** refuses it (`paused`). The activity log gets a `shelf:` line for it.
- **Any other `xfer.get` of a photo** goes through the hands: a photo left over after a cut transfer, or asked for again. Their `read` posture applies: `ask` asks before the photo leaves, `off` refuses it. With the hands off, or no usable policy, it is refused. In roots and in full mode alike, a photo leaves past the deny list, which holds `~/Library`.
- Only a regular file with one name, directly in that directory and with a photo's name, leaves either way; neither the directory nor the file may be a symlink.
- A photo leaves once. When the receiver's verdict names the photo's size and sha256, Tulpa deletes it from the Mac. After any other verdict, or a cut transfer, it stays.
- **A photo not fetched is deleted once it is 24 h old**, the rule for a `.part`: at Tulpa's start and with each `eyes.photo`. Only photo-named regular files directly in the directory are deleted, never through a symlink. Until then the menu's eyes line counts it (`· 1 photo waiting`).
- With the eyes off in config, the directory is no shelf: a photo kept from before stays until it is 24 h old, and the eyes line shows while one waits (`eyes: off · 1 photo waiting`).

**On the hub (planned):** the hub client's `eyes.photo` for a device asks for the photo, pulls it with `xfer.get` into a private directory on the hub, and prints its path there. It waits up to 120 s for the photo.

## Ears modes

A mode does three things:

1. It picks the capture command: `[ears.modes].<mode>`, and for `call` the plain `[ears] command` when `call` has none. A mode without a command shows in the menu and is refused when picked.
2. It decides whether the system audio's lines leave the Mac (only `call`'s do).
3. It tells the agent the situation: the mode is in every transcript line, in the `mode` and `switched` events, and in `ears.status`.

The mode does not choose a side. The capture command decides what it records, and the self-check expects both sides in every mode. One script for every mode is fine.

**Which lines go: the side decides** (since 2026-11-18). The microphone's lines — `me` — go in every mode: they are the owner's own words, spoken to the agent. The system audio's lines — `them` — go only while a call runs, mode `call` or a call left in that mode: outside a call the system audio is a playing video, a nearby conversation, other people's words, and none of it leaves the Mac.

| Mode | Meant for | Lines that go out |
| --- | --- | --- |
| `off` | not listening | none: no capture runs |
| `alone` | alone with the owner | every `me` line; no `them` line |
| `office` | at the office | every `me` line; no `them` line |
| `meeting` | a live meeting in the room | every `me` line; no `them` line |
| `call` | a call: microphone and system audio | every line, either side |
| `stream` | a stream | every `me` line; no `them` line |

`me` is the microphone and `them` is the system audio: the other side of a call, or a video. The `mode` in each event says which rule met it.

How lines are read:

- From the first capture start on, Tulpa reads `[ears] transcript_files` every 500 ms and takes only new whole lines. A line looks like `HH:MM:SS [me] [speaker] text`; the time and the speaker tag are optional, and a line without `[me]` or `[them]` never goes.
- The time is UTC. When it lies within 5 minutes of the Mac's clock it becomes the event's `ts`; otherwise `ts` is the time Tulpa read the line.
- The speaker tag is one word of 1 to 32 ASCII letters, digits, `_` or `-` in brackets right after the side, such as `[owner]` or `[other]`. The event carries it as `speaker`, and its text leaves the tag out. Anything else in brackets there, such as `[laughs softly]`, `[own:er]` or a longer word, is not a tag: it stays in the text, and the event has no `speaker`.
- A `them` line is read with the mode of the moment it is read: a line that arrives while the mode is no longer `call` stays on the Mac, so the last lines of a call never leave as another mode's.
- Lines written while paused or while no capture runs are dropped, never sent later.
- The event's text is cut to 4000 characters, and the log keeps only its side, mode and length.

The keys that gated lines before — `wake_words`, `wake_aliases`, `transcript_events`, `followup_secs`, `followup_max_secs` — still load, so an old config keeps working, but they do nothing now; a set one is noted in the log once at startup.

### The capture command

The bundle ships two captures. Both print the same lines (`TULPA-EARS-LINE`, see `config.example.toml`), keep each side's directory as `<state>/me` and `<state>/them` for the self-check, and keep the raw ring, the last few MiB of recognised audio, in `/tmp/tulpa-ears-ring-<uid>` (`0600`). `<state>` is `EARS_STATE`, by default `~/.cache/tulpa-ears`.

- `ears-capture.sh`, the default: a shell script around ffmpeg, the system-audio helper, `tulpa-segment`, `whisper-cli` and awk. It needs Homebrew's `ffmpeg` and `whisper-cpp` 1.9.5 or 1.9.6 (the versions whose `-ojf` layout the transcript reader was checked against; any other refuses). Every piece it prints as `<voice N.Ns>` (heard, not read) also leaves one `TULPA-EARS-DEBUG [<side>] voice-unread:<rule> nt=.. m=.. maxrun=.. ms=..` line in `ears.log` (stderr when it writes transcript files): the rule that decided it (`no-tokens`, `markers-only`, `empty-text`, `credit-leak` with its `family=`, `maxrun`, `assembler-failed`, or `unknown`) and its counts, never the piece's text. A subtitle-credit line costs only its own words: the rest of the piece is published, and the piece leaves `TULPA-EARS-DEBUG [<side>] credit-dropped n=<lines> ms=.. family=<families>` instead; only a piece of nothing but credit lines is `<voice>` as `credit-leak`. The tray sends none of these records over the link.
- `tulpa-hear --ears`: the same ears in one process. The microphone, the system-audio tap and whisper.cpp all run inside it, so it needs no Homebrew tools. It loads the model once at start, which takes 15 to 20 s. It also keeps `<state>/heard.log` (`0600`): one line per phrase and per unread piece, with the reason it was unread. An unread phrase shows as `<voice N.Ns>`: voice heard but not read. A piece in which whisper found no speech at all (mostly room noise) shows as `<no speech N.Ns>` instead, with the outcome `emitted:no-speech`; its piece's own line says `no-speech` where an unread voice piece's says `voice-unread:<reason>`. After 300 s (`EARS_DEAF_S`) of one side's pieces in a row that whisper read as nothing but its own markers (`markers-only`) or as blank text (`empty-text`), it prints one `<deaf-suspect …>` line. Other unread pieces (no tokens at all, a subtitle-credit line, a decode loop) stop that count, and only a piece read as text lets it fire again. With `debug = true` at the top of the config (off by default), it also writes one `TULPA-EARS-DEBUG` line per recognized piece into `ears.log`: whisper's raw text before the credit filter, each pass's language (and the second pass with language detection, when the first was credits only) and the filter's verdict. Those lines stay in `ears.log` and never go to the agent.

Both read the whisper model and the VAD model from `EARS_MODELS`, by default `~/.cache/calltx`: `ggml-large-v3-turbo-q5_0.bin` and `ggml-silero-v5.1.2.bin`.

To switch to `tulpa-hear`, change the config and nothing else:

```toml
[ears]
command = ["/Applications/Tulpa.app/Contents/MacOS/tulpa-hear", "--ears"]
me_dir = "~/.cache/tulpa-ears/me"
them_dir = "~/.cache/tulpa-ears/them"
# err_file: remove it. tulpa-hear writes its errors to ears.log, and an old
# err_file of the script's would still be read.
```

Do the same in every `[ears.modes]` entry that runs the script. Set `me_dir` and `them_dir` to your `EARS_STATE` if you set one. Then pick the mode again in the menu, or send `ears.mode <mode>`: the new command starts with the next capture. To go back, put the script's command back.

Before the switch, `/Applications/Tulpa.app/Contents/MacOS/tulpa-hear --check` says whether this Mac is ready: macOS 14.6 or later, both model files, and a build with whisper. `tulpa-hear --ears` runs the same checks at start and exits 1 when one fails; its reason is in `ears.log`.

What differs:

- `tulpa-hear` follows the default microphone by itself. A microphone picked in the menu pins it, as with the script.
- The first time, macOS asks for the microphone and for System Audio Recording for Tulpa. Until the second is granted, the system-audio side stays silent and the self-check reports it DEGRADED.
- The speaker tagger (`EARS_TAG`) and the transcript files of a capture without `TULPA_LINES_STDOUT` exist only in the script.

### The mode after a restart

Tulpa keeps the mode last chosen in `~/.config/tulpa/ears-mode`, beside the config file: one line, the mode's id, replaced whole. At start it switches to that mode, if `[ears]` is on and the mode still has a capture command, and logs `ears: mode <id> restored after restart`. The agent gets the `mode` event once the link is up. A saved `off`, a missing or unknown file, a mode that is no longer configured, or a capture that does not start leaves the ears off; the log says why, except for a saved `off`.

| What happens | What the file gets |
| --- | --- |
| A mode picked in the menu, or `ears.mode <mode>`, including the mode already on | that mode |
| `ears.start` with no capture running | `call` |
| `ears.stop`, `ears.mode off`, **Pause all** | `off` |
| `ears.start` while a capture runs | nothing |
| A switch refused: paused, or the mode not configured | nothing |
| A switch whose capture does not start: the old capture is already stopped, so the ears are off | nothing: the file keeps the mode from before |
| A capture that exits by itself, or a new microphone whose restart fails | nothing |
| **Quit**, a logout, or a signal that stops Tulpa (SIGTERM, SIGINT or SIGHUP: `launchctl kickstart -k`, the dead-man's relaunch, a second Tulpa taking over) | nothing |

So `off` is saved only by `ears.stop`, `ears.mode off` and Pause, never by Quit. After a Quit or a crash, the next start comes back in the mode last chosen. Under the installer's LaunchAgent, launchd makes that next start by itself after a crash or a signal, but not after Quit: then the next start is when Tulpa is opened again (see [the menu](#the-menu)). To have the ears come back off, send `ears.stop` or Pause first, or delete the file.

## Hands modes

Two settings decide what the hands may do.

`[hands] mode` in `~/.config/tulpa/config.toml`:

- `off`: every hands verb is refused (`verb`).
- `on` or `ask`: the policy file decides (`[hands] policy`, usually `~/.config/tulpa/hands.toml`).
- There is no `full` value. Any word other than `off`, `ask` or `on` makes the whole config invalid, and Tulpa does not start.

Edit the policy file on the Mac. Outside full mode the hands can neither read nor write it: it is on their deny list. Tulpa reads it once at start, so an edit applies after a restart.

| Hands mode | How it is set | What it allows |
| --- | --- | --- |
| off | `[hands] mode = "off"` | nothing |
| on (named only) | `[hands] mode = "on"`, and `full = false` or absent in the policy | `read`, `list` and `git.*` inside the policy's `roots`, and `run` for the policy's named commands. `shell` and `write` answer `<verb>: the hands are not in full mode`. |
| full | `full = true` in the policy, or **Hands: full** in the menu | `shell`, `write`, and `read`, `list`, `xfer.put` and `xfer.get` at any path, as the user: no roots, no deny list, no asking. `run` and `git.*` still follow the policy. |

Inside the on mode, each verb group has a posture of its own in the policy:

| Policy key | Covers | Values | Default |
| --- | --- | --- | --- |
| `read`, `list`, `git` | `read`, `list`, `git.status`, `git.diff`, `git.log`; `read` also `xfer.get` | `off`, `ask`, `on` | `on` |
| `put` | `xfer.put` | `off`, `ask`, `on` | `ask` |
| `[[command]]` `mode` | one `run <name>` | `off`, `ask`, `on` | `ask` |
| `ask_timeout_secs` | how long a question waits | 1 to 900 | 30 |

- `on` runs at once; `off` refuses (`off`).
- `ask` asks the person at the Mac in [the Allow/Deny dialog](#the-allowdeny-dialog). No click in time: `no click in time`. A deny is refused too. Pause denies every waiting question.
- The deny list wins over every root: `~/.ssh`, `~/Library`, `~/.config/tulpa`, key files, shell histories, credentials and similar. The full list is in `crates/tulpa-hands/src/deny.rs`. The one exception is the eyes' photo directory, for `xfer.get` only ([Eyes](#eyes)).
- **Hands: full** in the menu writes `full = …` into the policy file, so a restart keeps it. If that write fails, the log says so and a restart reverts it.
- Every request is one line in the activity log, with its decision and result. A `write` logs its path and size, never its content.

## The Allow/Deny dialog

A request whose posture is `ask` waits for the person at the Mac: a hands verb, `xfer.put`, or `eyes.photo` with `[eyes] mode = "ask"`. The dialog is built and tested with a stand-in for the window; it has not run on a Mac yet.

- **The dialog** comes up in front of the other apps' windows, on the Space in use, in a window titled Tulpa. It shows the question, the time it was asked and how long it waits. **Deny** is the default: Return presses it. **Allow** is greyed out for the first second, so a click meant for something else, as the dialog appears, can't allow. Escape is not bound to either button.
- **The menu** shows the same question with **Allow** and **Deny**. A click there answers that question and closes the dialog. A click in a menu opened before its question went answers nothing.
- **A notification** says `<question> — answer in the Tulpa dialog`. It is a heads-up only: a click on it opens Script Editor and answers nothing.
- **No answer** within `ask_timeout_secs` (30 s by default): the dialog closes, and the request answers `no click in time`.
- **One dialog at a time.** A second question waits behind the first, and the menu line says `· 1 more waiting`. It shows when the first is answered or expired. Each question's time runs from when it was asked, so one can run out while it waits. At most 8 wait; one more is refused at once with `no click in time`.
- **Pause all** and **Quit** deny every waiting question and close the dialog.
- The dialog and the menu show a control character, or one that reorders text, as `�`, and cut a question after 400 characters. The activity log keeps the whole request.

## Events

Tulpa also speaks unasked: it pushes event lines with no `id` over the link. While the link is down, the newest 16 wait for the next connection; older ones are dropped.

| Event | When |
| --- | --- |
| `{"event":"ears","state":"mode","mode":"alone","from":"off","source":"menu"}` | The mode changed: the menu, `ears.mode`, `ears.start`, `ears.stop`, Pause, a capture that exited, a microphone change whose restart failed, or a start that restored the saved mode. `source` says which: `menu`, `request:<id>`, `pause`, `exited`, `mic`, `restore`. |
| `{"event":"ears","state":"mode","mode":"alone","from":"alone","source":"link-up"}` | Every time the link comes up: the current mode, `off` included, with `from` the same. It goes after the lines that waited while the link was down, so the last `mode` line is always the current mode. |
| `{"event":"ears","state":"switched","mode":"alone","from":"off","check":"ok"}` | Within 15 s of a switch to a mode other than `off`. `ok`: both sides made a chunk. `degraded`: one side is silent (with `side`, `reason`, `fix`); the capture runs on. `failed`: neither side made a chunk. |
| `{"event":"ears","state":"degraded","side":"them","reason":"…","fix":"…"}` | The self-check found one side failing. |
| `{"event":"ears","state":"deaf","reason":"…"}` | The self-check found both sides failing. |
| `{"event":"ears","state":"running","reason":"hearing again"}` | Back after `degraded` (with `side`) or `deaf`. |
| `{"event":"ears","state":"exited","code":1}` | The capture exited by itself. |
| `{"event":"ears","state":"off","reason":"paused"}` | Pause killed the capture. |
| `{"event":"ears","state":"line","side":"me","speaker":"owner","mode":"alone","text":"…","ts":"2026-01-15T09:30:00Z"}` | A transcript line that may go (see [Ears modes](#ears-modes)). `speaker` is there only when the line carries a speaker tag. `ears.test` sends the same shape with `"test":true` and no `speaker`. |
| `{"event":"link","state":"direct-failed","tried":["…"]}` | On an office Wi-Fi no direct address answered, so the link went through the jump host. |

A state that holds sends no new event: a new reason in the same state only updates the menu. A switch of the failing side, `them` to `me`, is a new event.

### On the hub

The hub is not part of this repository. The hub in use turns each event into a short notice without the transcript text, such as `tulpa ears: deaf …`.

The hub in use also does the following. It is the hub's code, not this repository's:

- The hub publishes each event on its bus for one hour, as `senses.dev.<device>.<organ>.<kind>`:

  | Event | Subject |
  | --- | --- |
  | ears `line` | `senses.dev.<device>.ears.line`, with `device`, `side`, `mode`, `text`, `ts`, and `test` for a test line |
  | ears, any other state | `senses.dev.<device>.ears.state`, with `state`, `mode`, `from`, `check`, `reason`, `code` when present |
  | link, including the hub's own online and offline | `senses.dev.<device>.link.state` |
  | the hub's call handshake | `senses.dev.<device>.call.topic` |
  | hands | `senses.dev.<device>.hands.state` (Tulpa sends no hands events yet) |

- **Subscribe** with the hub client: `wake-on --nats <subject> --match <regex> [--field <name>]`. The subject stays under `senses.dev.`, with `*` for one part or a final `>`, for example `senses.dev.*.ears.line` or `senses.dev.<device>.>`. The regex matches in any case and is checked for runaway patterns. `--field` names the payload field to test, `text` by default. Each match wakes the subscriber with `<device> (<side>): <value>`, the side only for transcript lines; a test line's wake starts with `[test] `.
- **Grant:** a subscription that could match `ears.line` or `call.topic` needs the `ears.listen` grant in the hub's per-caller grants. The hub's administrators have it. State subjects stay open to every caller. The grant is checked again at each wake, and a transcript wake is private to its subscriber.
- **Test** with the hub client: `test <device> ears`. It sends `ears.test` with a random marker and waits up to 30 s for that marker on `senses.dev.<device>.ears.line`. It prints `ears test: ok, <ms> ms (<device>)`, or `ears test: no line in 30 s` and exits nonzero.

The hub may also shape what reaches the agent, beyond what Tulpa sends: it may pace and cap the wakes of each subscription, pass on only once a phrase that two Macs in one room both heard, and hold back a line that is the agent's own voice heard through a microphone. That is the server's work, not Tulpa's.

## DEGRADED and DEAF

Tulpa checks the capture 15 s after a start, then every 30 s while it runs. It judges each side on its own:

- `me` looks at `[ears] me_dir` and `them` at `[ears] them_dir`. A side fails when its directory has had no new file change for 20 s, counted from the capture's start when the directory has not changed since: a change from before the start is the previous capture's. So a capture that takes time to start, such as `tulpa-hear --ears` loading its model, is never reported failing before it has run 20 s. Its reason reads `no chunk rotated for 45 s`, and `no chunk rotated for 45 s (lid closed)` when the Mac's lid is shut. Tulpa reads the lid from the I/O Registry (`AppleClamshellState`, with `/usr/sbin/ioreg`, 5 s at most) only when a side has stopped; a Mac without a lid, or an `ioreg` that fails, never says it. A closed lid stops both sides, so it usually shows as DEAF: `me: no chunk rotated for 45 s (lid closed); them: no chunk rotated for 45 s (lid closed)`.
- A side also fails when the newest line about it in `[ears] err_file` reads as an error (`error`, `denied`, `failed`, `cannot`, `no display` and similar). A line is about `me` when it starts `me:` or `mic:` or names the microphone. It is about `them` when it starts `them:` or `sysaudio:` or names system audio or screen capture. A line that names neither side, or both, counts for both.

| State | Meaning | Menu line |
| --- | --- | --- |
| running | Both sides capture. | `ears: listening (pid N)` |
| DEGRADED | One side fails. The capture keeps running and the other side is still heard. | `ears: DEGRADED — them: <reason> (me still heard) — fix in <pane> (click to open)` |
| DEAF | Both sides fail. The capture process still runs, but nothing is heard. | `ears: DEAF — <reason>` |
| exited | The capture ended by itself; the mode is off. | `ears: capture exited (N)`, or `ears: capture killed` when a signal from elsewhere ended it |

Tulpa never stops the capture because of a check, and the state returns to running by itself when chunks flow again.

**When you see DEGRADED:**

- Keep the ears on. Do not send `ears.stop` or `ears.mode off`: the other side is still heard. Tell the person at the Mac which side is lost if it matters.
- `them` (system audio): Tulpa lacks **Screen & System Audio Recording**, or the system-audio tool stopped because the display sleeps or the screen is locked. A tool built on ScreenCaptureKit stops then, and its error may read `no display`. Grant the permission in System Settings → Privacy & Security → Screen & System Audio Recording (a click on the menu line opens that pane), or wake and unlock the display. A capture script that restarts its system-audio tool brings the side back by itself. `-d` in `[keepawake] flags` keeps the display awake; a management policy may still lock the screen. A fallback that takes the system audio through a Core Audio process tap is being investigated (in progress, not in this repository).
- `me` (microphone): Tulpa lacks **Microphone**, or the picked microphone is gone. Grant it in Privacy & Security → Microphone. Check **Ears mic**: a missing device shows `(not connected)`; pick another one or Default.
- A switch reported with `"check":"degraded"` is a switch that worked.

**When you see DEAF:**

- Read the reason: the menu line, `ears.status` or the event. One shared error line gives one reason; otherwise there is a reason per side.
- `(lid closed)` in the reason: the lid is shut, and the capture stops with it. Nothing is broken. The ears come back once a person at the Mac opens the lid.
- Look at `~/Library/Logs/tulpa/ears.log` (the capture's own output) and at the error file.
- Restart the capture: `ears.mode off`, then the mode again. Asking for the current mode does not restart it.
- After a permission change, the capture may need that restart, or Tulpa may need a restart.
- If it stays deaf, a person at the Mac checks the audio devices and both grants.
- Test the microphone only through Tulpa's own capture. A tool run over SSH has none of Tulpa's grants, so it measures nothing about Tulpa.

**When you see exited:** the capture command ended. Read `ears.log` for why, then start the mode again.

## The menu

From top to bottom:

| Row | What it shows or does |
| --- | --- |
| `link: connected via <alias>` | The link: `off`, `connecting` or `connected`. |
| `link: direct (<host>)` or `link: via <alias>` | The route. `⚠ direct did not come up` means the office network's direct route failed. |
| A question with **Allow** and **Deny** | The question first in line, the one [the dialog](#the-allowdeny-dialog) shows, with the time it was asked and how many more wait. A click answers that question only. Shown only while one waits. |
| `hands: on (N roots) · running: 0` | The hands' state and how many requests run now. Also `hands: off`, `hands: paused`, or `hands: off (no usable policy, see log)`. |
| **Hands: full** | Turns full mode on or off. Shown when a hands policy is loaded. |
| **Ears: <mode>** | Picks the ears mode, like `ears.mode`. Shown when `[ears]` is on. |
| The ears line | `ears: listening (pid N)`, `ears: ready (not listening)`, `ears: paused`, DEGRADED, DEAF or exited; `ears: off` when `[ears]` is off or unusable. A click on a DEGRADED line opens the Settings pane that fixes it. |
| `eyes: camera on request` | The eyes, while `[eyes] mode` is `on` or `ask` (`, asks first`): also `eyes: taking a photo` while the helper runs, `eyes: paused`, the time of the last photo kept, and the photos waiting to be fetched (`· 2 photos waiting`). With the eyes off, the line shows only while a photo waits: `eyes: off · 1 photo waiting`. |
| **Ears mic: <name>**, **Voice output: <name>** | The Mac's inputs and outputs by name, and Default, read again when the pointer reaches the menu-bar icon. Shown when `[ears]` is on. The pick is written into the config. A new microphone restarts a running capture in its mode. A microphone whose name has a `:`, or starts with a digit (leading spaces or a `+` aside), is greyed out and marked `use the capture script for this one`. A pick of it is refused, and the config stays as it was: the capture's ffmpeg would cut the name at the `:` or read the digit as a device index, and record another input. Name such a device in the capture script instead. A name typed into `[ears] mic_device` by hand is not checked this way. |
| `keep-awake: on (always, -is)` | Tulpa's own keep-awake, a `caffeinate` child with the flags `[keepawake] flags` names (`-i -s` by default, shown as `-is`). By default it holds for as long as Tulpa runs. The parentheses list why it holds: `always`, and `ears` while a capture runs or `hands` while a hands request is at work. With `[keepawake] always = false`, only `ears` and `hands` hold it, and it ends 120 s after the last one (`on (finishing, -is)`, then `off`). `FAILED (always: caffeinate is not running)`: it is started again after 1 s, then up to 60 s apart while it keeps ending. `ears.status` carries the same text as `keepawake`. |
| `53682 → once — click to close` | An open port forward, one row each: `→` out, `←` in. A click closes it. |
| **Open config** | Opens `config.toml` in the default editor. |
| **Pause all** | Pauses every sense: hands verbs refused, a running command killed, every waiting question denied and its dialog closed, the capture killed, `voice.say` and `eyes.photo` refused, a photo being taken killed, a transfer closed, every port forward closed. The saved ears mode becomes off. Resume does not restart the capture; ask for the mode again. There is no hotkey yet. |
| **Quit** | Stops everything and exits. Tulpa stays down until it is opened again. It leaves the saved ears mode as it is. |

**After Quit nothing listens** until Tulpa is opened again. Quit exits with status 0, and the installer's LaunchAgent (`KeepAlive = { SuccessfulExit = false }`) starts Tulpa again by itself only after any other end: a crash or a kill. Tulpa starts again at the next login, or at once when a person at the Mac runs `launchctl kickstart gui/$(id -u)/com.tulpa.app`. Opened from `~/Applications` instead, it runs outside the LaunchAgent until the next login, and launchd does not start it again after a crash. The next start comes back in the saved ears mode: to have the ears come back off, use Pause or `ears.stop` before Quit; both save the mode as off.

## Where things are on the Mac

| Path | What |
| --- | --- |
| `~/.config/tulpa/config.toml` | The config. `config.example.toml` in this repository documents every key. |
| `~/.config/tulpa/hands.toml` | The hands policy (`hands.example.toml`). |
| `~/.config/tulpa/ears-mode` | The mode the ears come back in after a restart ([what writes it](#the-mode-after-a-restart)). Delete it to start with the ears off. |
| `~/Library/Application Support/tulpa/photos/` | The eyes' photos until they are fetched or 24 h old (the directory `0700`, each photo `0600`). |
| `~/Library/Logs/tulpa/tulpa.log` | The activity log: one line per request and per change. A `write`'s content, spoken text and transcript text never go in it. |
| `~/Library/Logs/tulpa/ears.log` | The capture command's own output. |
