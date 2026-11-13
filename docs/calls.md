# Calls and the meeting assistant

Design only; no capture implementation exists yet. Calls are the first full organ after the SSH spike. Every call starts from an explicit human request, and pause or stop immediately ends all capture.

## Data flow

Keep two timestamped streams throughout the pipeline:

- **Remote:** output audio from the selected call app.
- **Local:** the human's microphone, captured separately by the ears helper.

Apply VAD and segment merging independently, then produce partial and final transcripts with stream labels. The local stream is the human by construction. Run speaker diarization locally on the remote stream because it may contain several people; never treat the whole remote stream as one speaker by default.

Send live partials and finals to the agent over the link. Meeting assistance is quiet text in a local overlay or notification, never voice unless explicitly requested. After the call, the agent produces notes and action items and stores them only in a location chosen by the human. Joining by URL or answering an incoming call is a separate explicit action, not permission to begin recording automatically.

## Capturing the call app's audio

### ScreenCaptureKit

Available for audio on macOS 13 and later. Set `SCStreamConfiguration.capturesAudio`, register an `SCStreamOutput` for `.audio`, and receive audio sample buffers. Video output is not required for the calls pipeline.

Use an `SCContentFilter` to select the call window or the windows belonging to the call app. `excludingCurrentProcessAudio` should normally be true so tulpa does not capture its own playback. This is a natural fit when call capture should follow a particular visible window.

Limitations to prove before implementation:

- A browser-based call may share process audio with other tabs. Window filtering does not by itself prove tab-only audio isolation.
- An app filter is not the same as a process-ID filter, and a call can move windows or recreate them.
- Permission changes can require restarting capture or the app.

Apple's [screen and audio capture sample](https://developer.apple.com/documentation/screencapturekit/capturing-screen-content-in-macos) demonstrates both outputs and the Screen Recording prompt. The [`capturesAudio`](https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration/capturesaudio) property and [`.audio`](https://developer.apple.com/documentation/screencapturekit/scstreamoutputtype/audio) output define the audio path.

### Core Audio process taps

Use this alternative for process-oriented capture, especially when the call has no stable visible window. Resolve the selected app's processes to Core Audio process objects, create a private `CATapDescription` for those processes, and pass it to `AudioHardwareCreateProcessTap`. Add the tap to a private aggregate audio device and read it as an input device.

The tap must not mute the call's normal speaker output. Destroy the aggregate device, tap, and active I/O state on pause, stop, call exit, or app selection changes.

The requested product floor is macOS 14.4 or later. Current Apple documentation actually marks [`AudioHardwareCreateProcessTap`](https://developer.apple.com/documentation/coreaudio/audiohardwarecreateprocesstap(_:_:)) and `NSAudioCaptureUsageDescription` as 14.2+, but no earlier support is needed or claimed.

Apple's [Core Audio taps sample](https://developer.apple.com/documentation/coreaudio/capturing-system-audio-with-core-audio-taps) documents the tap plus aggregate-device route and its permission prompt. A process tap still captures the selected browser app's audio as a whole; it does not provide per-tab isolation either.

## TCC grants and process packaging

| Capture source | Required user grant and declaration |
| --- | --- |
| ScreenCaptureKit app/system audio, even without video output | **Screen Recording** for the app |
| Core Audio process tap through an aggregate device | **System audio recording** permission, with `NSAudioCaptureUsageDescription` in the app bundle's `Info.plist`; the prompt occurs when recording from the tapped device starts |
| Human microphone with either route | **Microphone**, with `NSMicrophoneUsageDescription` in the responsible app's `Info.plist` |
| Camera, if later requested for calls | Separate **Camera** grant; never implied by audio or screen permission |

ScreenCaptureKit does not need `NSAudioCaptureUsageDescription`; Core Audio taps do not need Screen Recording unless the product also captures video through ScreenCaptureKit. Microphone access remains separate in every design. Missing microphone usage text can terminate the process when access is requested.

The Core Audio sample requires `Info.plist` usage text, and both frameworks need reliable TCC ownership. Therefore the product path is the **signed tulpa app bundle**, with the microphone helper inside it. A CLI can technically call these C/Objective-C APIs, but it is only a development probe: without bundle usage text and stable code identity, the grant can attach to Terminal or another responsible process, fail, or behave differently across launches. Do not use a Terminal-owned grant as evidence that the bundled app works.

Neither audio route needs a virtual audio driver. Core Audio's system-audio grant is separate from the restricted Keychain entitlement used by the SSH spike; one does not authorize the other.

## Recommended direction

Start with **Core Audio process taps inside the signed bundle** when reliable app/process selection is available, because it does not require a visible call window. Keep ScreenCaptureKit as the alternative when window-scoped selection is preferable or its capture semantics work better for a specific call app.

Before choosing either as the default, run a small bundled probe that proves:

1. Only the intended call app is captured, including the browser multi-tab case.
2. The human still hears the call; the tap does not mute playback.
3. Screen Recording, system-audio, and microphone grants belong to tulpa rather than Terminal.
4. Pause stops remote and local capture and clears the indicators within one second.
5. No audio is written or sent unless recording was explicitly requested, and all notes remain inside the chosen storage root.
