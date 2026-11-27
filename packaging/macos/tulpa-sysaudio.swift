// tulpa-sysaudio — the ears' system-audio side: ScreenCaptureKit → s16le 16 kHz mono on stdout.
// Same mechanism OBS's "macOS Audio Capture" uses (2026-11-11). Built into the app bundle by
// scripts/bundle-macos.sh and run by packaging/macos/ears-capture.sh as its own child, so the
// Screen Recording grant is the app's; it dies with "no display" while the display sleeps and the
// capture script restarts it. Errors and exit codes: 1 capture setup, 2 stopped with error,
// 3 no display, 0 stdout closed.
import Foundation
import ScreenCaptureKit
import CoreMedia
import AVFoundation

final class Out: NSObject, SCStreamOutput, SCStreamDelegate {
    let fh = FileHandle.standardOutput
    func stream(_ s: SCStream, didOutputSampleBuffer sb: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .audio, sb.isValid else { return }
        var abl = AudioBufferList(); var block: CMBlockBuffer?
        let st = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sb, bufferListSizeNeededOut: nil, bufferListOut: &abl,
            bufferListSize: MemoryLayout<AudioBufferList>.size, blockBufferAllocator: nil, blockBufferMemoryAllocator: nil,
            flags: kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, blockBufferOut: &block)
        guard st == noErr, let data = abl.mBuffers.mData else { return }
        let n = Int(abl.mBuffers.mDataByteSize) / 4
        let f = data.bindMemory(to: Float32.self, capacity: n)
        var out = [Int16](repeating: 0, count: n)
        for i in 0..<n { out[i] = Int16(max(-1, min(1, f[i])) * 32767) }
        out.withUnsafeBytes { b in
            var off = 0
            while off < b.count { let r = write(1, b.baseAddress! + off, b.count - off); if r <= 0 { exit(0) }; off += r }
        }
    }
    func stream(_ s: SCStream, didStopWithError e: Error) { FileHandle.standardError.write("sysaudio: stopped: \(e)\n".data(using: .utf8)!); exit(2) }
}

let out = Out()
var keep: SCStream?
Task {
    do {
        let c = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false)
        guard let d = c.displays.first else { FileHandle.standardError.write("sysaudio: no display\n".data(using: .utf8)!); exit(3) }
        let filter = SCContentFilter(display: d, excludingWindows: [])
        let cfg = SCStreamConfiguration()
        cfg.capturesAudio = true; cfg.excludesCurrentProcessAudio = true
        cfg.sampleRate = 16000; cfg.channelCount = 1
        cfg.width = 2; cfg.height = 2; cfg.minimumFrameInterval = CMTime(value: 1, timescale: 1)
        let s = SCStream(filter: filter, configuration: cfg, delegate: out)
        try s.addStreamOutput(out, type: .audio, sampleHandlerQueue: DispatchQueue(label: "a"))
        try await s.startCapture(); keep = s
        FileHandle.standardError.write("sysaudio: capturing system audio, s16le 16k mono\n".data(using: .utf8)!)
    } catch { FileHandle.standardError.write("sysaudio: \(error)\n".data(using: .utf8)!); exit(1) }
}
signal(SIGPIPE, SIG_IGN)
RunLoop.main.run()
