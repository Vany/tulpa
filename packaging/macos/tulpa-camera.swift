// tulpa-camera: one still from the Mac's default camera, as JPEG on stdout.
//
// Tulpa runs it as its own child for the link verb eyes.photo, so macOS
// attributes the camera to Tulpa: the prompt names Tulpa, and the grant is
// Tulpa's (System Settings > Privacy & Security > Camera). The camera light
// is macOS's. It takes no arguments and keeps nothing: Tulpa writes the file.
//
// Exit 0 with the JPEG on stdout, or nonzero with one "camera: ..." line on
// stderr: 2 an argument was given, 3 not allowed, 4 no camera, 5 the capture
// failed, 6 its own deadline passed. Before the macOS prompt it prints
// "camera: asking macOS for access" (Tulpa's eyes.rs reads that line).
//
// Built and signed into Tulpa.app by scripts/bundle-macos.sh (swiftc, Swift 5
// language mode, macOS 11 or later).

import AVFoundation
import Foundation

/// How long the camera runs before the still is taken, so its exposure settles.
let settleSeconds = 1.5
/// The helper's own deadline, for when Tulpa is gone and cannot stop it.
let ownLimitSeconds = 90.0

func say(_ line: String) {
    FileHandle.standardError.write(Data("camera: \(line)\n".utf8))
}

func fail(_ code: Int32, _ line: String) -> Never {
    say(line)
    exit(code)
}

/// Receives the one photo, hands its JPEG over on stdout, and ends the process.
final class Still: NSObject, AVCapturePhotoCaptureDelegate {
    func photoOutput(
        _ output: AVCapturePhotoOutput,
        didFinishProcessingPhoto photo: AVCapturePhoto,
        error: Error?
    ) {
        if let error = error {
            fail(5, "the capture failed: \(error.localizedDescription)")
        }
        guard let jpeg = photo.fileDataRepresentation() else {
            fail(5, "the capture gave no image")
        }
        do {
            try FileHandle.standardOutput.write(contentsOf: jpeg)
        } catch {
            fail(5, "the photo could not be handed over: \(error.localizedDescription)")
        }
        exit(0)
    }
}

let session = AVCaptureSession()
let output = AVCapturePhotoOutput()
let still = Still()
let queue = DispatchQueue(label: "tulpa-camera")

/// Opens the default camera, lets it run for a moment, and takes one still.
func shoot() {
    queue.async {
        guard let camera = AVCaptureDevice.default(for: .video) else {
            fail(4, "no camera")
        }
        let input: AVCaptureDeviceInput
        do {
            input = try AVCaptureDeviceInput(device: camera)
        } catch {
            fail(5, "the camera did not open: \(error.localizedDescription)")
        }
        session.beginConfiguration()
        guard session.canAddInput(input), session.canAddOutput(output) else {
            fail(5, "the camera cannot take a still")
        }
        session.addInput(input)
        session.addOutput(output)
        if session.canSetSessionPreset(.photo) {
            session.sessionPreset = .photo
        }
        session.commitConfiguration()
        guard output.availablePhotoCodecTypes.contains(.jpeg) else {
            fail(5, "the camera cannot make a JPEG")
        }
        session.startRunning()
        queue.asyncAfter(deadline: .now() + settleSeconds) {
            let settings = AVCapturePhotoSettings(format: [AVVideoCodecKey: AVVideoCodecType.jpeg])
            output.capturePhoto(with: settings, delegate: still)
        }
    }
}

if CommandLine.arguments.count > 1 {
    fail(2, "no arguments are taken")
}
// A closed stdout fails the write above instead of killing the process silently.
signal(SIGPIPE, SIG_IGN)
// The deadline is a timer on the main run loop, which also keeps that loop running.
_ = Timer.scheduledTimer(withTimeInterval: ownLimitSeconds, repeats: false) { _ in
    fail(6, "no photo in \(Int(ownLimitSeconds)) s")
}
let denied = "denied (System Settings > Privacy & Security > Camera: allow Tulpa)"
switch AVCaptureDevice.authorizationStatus(for: .video) {
case .authorized:
    shoot()
case .notDetermined:
    say("asking macOS for access")
    AVCaptureDevice.requestAccess(for: .video) { granted in
        if granted {
            shoot()
        } else {
            fail(3, denied)
        }
    }
default:
    fail(3, denied)
}
RunLoop.main.run()
