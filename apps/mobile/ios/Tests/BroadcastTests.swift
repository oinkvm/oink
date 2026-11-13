import XCTest
import CoreMedia
import AudioToolbox
@testable import OinkAudioTestHost

final class BroadcastTests: XCTestCase {
    private func withPipe(_ run: (OinkBroadcastPipe, OinkBroadcastPipe) throws -> Void) throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        try run(XCTUnwrap(OinkBroadcastPipe(root: root.path)), XCTUnwrap(OinkBroadcastPipe(root: root.path)))
    }
    func testReservationsAndBroadcastLifetime() throws {
        try withPipe { writer, reader in
            XCTAssertFalse(reader.broadcasting)
            XCTAssertTrue(writer.claimBroadcast())
            XCTAssertTrue(reader.broadcasting)
            XCTAssertFalse(reader.claimBroadcast())
            XCTAssertTrue(reader.claimApp())
            XCTAssertTrue(writer.appRequested)
            reader.releaseApp()
            XCTAssertFalse(writer.appRequested)
            writer.paused = true
            XCTAssertTrue(reader.paused)
            reader.stopRequested = true
            XCTAssertTrue(writer.stopRequested)
            writer.endBroadcast()
            XCTAssertFalse(reader.broadcasting)
        }
    }
    func testRingIsBoundedAndMarksDroppedBlocks() throws {
        try withPipe { writer, reader in
            XCTAssertTrue(writer.claimBroadcast())
            let pcm: [Float] = [0.25, -0.5]
            pcm.withUnsafeBufferPointer { p in
                for _ in 0..<30 { writer.writePCM(p.baseAddress!, frames: 1, rate: 44100, channels: 2, hostNS: 0, discontinuity: false) }
            }
            var count = 0
            reader.drain { p, frames, rate, channels, host, gap in
                XCTAssertEqual(Array(UnsafeBufferPointer(start: p, count: 2)), pcm)
                XCTAssertEqual(frames, 1); XCTAssertEqual(rate, 44100); XCTAssertEqual(channels, 2)
                XCTAssertEqual(host, 0)
                if count == 0 { XCTAssertTrue(gap) }
                count += 1
            }
            XCTAssertEqual(count, 24)
            pcm.withUnsafeBufferPointer { writer.writePCM($0.baseAddress!, frames: 1, rate: 48000, channels: 2, hostNS: 0, discontinuity: false) }
            reader.drain { _, _, rate, _, _, gap in XCTAssertEqual(rate, 48000); XCTAssertTrue(gap) }
        }
    }
    private func sample(bytes: [UInt8], bits: UInt32, flags: UInt32, rate: Double = 44100,
                        channels: UInt32 = 2) throws -> CMSampleBuffer {
        var asbd = AudioStreamBasicDescription(mSampleRate: rate, mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: flags, mBytesPerPacket: bits / 8 * (flags & kAudioFormatFlagIsNonInterleaved != 0 ? 1 : channels), mFramesPerPacket: 1,
            mBytesPerFrame: bits / 8 * (flags & kAudioFormatFlagIsNonInterleaved != 0 ? 1 : channels), mChannelsPerFrame: channels, mBitsPerChannel: bits, mReserved: 0)
        var format: CMAudioFormatDescription?
        XCTAssertEqual(CMAudioFormatDescriptionCreate(allocator: kCFAllocatorDefault, asbd: &asbd,
            layoutSize: 0, layout: nil, magicCookieSize: 0, magicCookie: nil, extensions: nil,
            formatDescriptionOut: &format), noErr)
        var block: CMBlockBuffer?
        XCTAssertEqual(CMBlockBufferCreateWithMemoryBlock(allocator: kCFAllocatorDefault, memoryBlock: nil,
            blockLength: bytes.count, blockAllocator: kCFAllocatorDefault, customBlockSource: nil,
            offsetToData: 0, dataLength: bytes.count, flags: 0, blockBufferOut: &block), noErr)
        let retainedBlock = try XCTUnwrap(block)
        bytes.withUnsafeBytes { p in XCTAssertEqual(CMBlockBufferReplaceDataBytes(with: p.baseAddress!,
            blockBuffer: retainedBlock, offsetIntoDestination: 0, dataLength: bytes.count), noErr) }
        var result: CMSampleBuffer?
        XCTAssertEqual(CMAudioSampleBufferCreateReadyWithPacketDescriptions(allocator: kCFAllocatorDefault,
            dataBuffer: retainedBlock, formatDescription: try XCTUnwrap(format),
            sampleCount: bytes.count / Int(bits / 8 * channels), presentationTimeStamp: CMTime(value: 1, timescale: 1),
            packetDescriptions: nil, sampleBufferOut: &result), noErr)
        return try XCTUnwrap(result)
    }
    func testBigEndianStereoInt16AndFormatChange() throws {
        let converter = OinkReplayPCM()
        let first = try sample(bytes: [0x40, 0, 0xc0, 0], bits: 16,
            flags: kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsBigEndian | kAudioFormatFlagIsPacked)
        var calls = 0
        XCTAssertTrue(converter.convert(first) { p, frames, rate, channels, host, _ in
            XCTAssertEqual(p[0], 0.5); XCTAssertEqual(p[1], -0.5)
            XCTAssertEqual(rate, 44100); XCTAssertEqual(frames, 1); XCTAssertEqual(channels, 2)
            XCTAssertEqual(host, 1_000_000_000); calls += 1
        })
        let second = try sample(bytes: [0, 0, 0, 0x3f], bits: 32,
            flags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked, rate: 48000, channels: 1)
        XCTAssertTrue(converter.convert(second) { p, _, rate, channels, _, _ in
            XCTAssertEqual(p[0], 0.5); XCTAssertEqual(rate, 48000); XCTAssertEqual(channels, 1); calls += 1
        })
        XCTAssertEqual(calls, 2)
    }
    func testPlanarStereoAndChunkTimestamps() throws {
        let converter = OinkReplayPCM()
        let planar = try sample(bytes: [0, 0x40, 0, 0xc0], bits: 16,
            flags: kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked | kAudioFormatFlagIsNonInterleaved,
            rate: 48000)
        var planarFrames = 0
        XCTAssertTrue(converter.convert(planar) { p, frames, _, channels, _, _ in
            XCTAssertEqual(p[0], 0.5); XCTAssertEqual(p[1], -0.5)
            XCTAssertEqual(channels, 2); planarFrames += Int(frames)
        })
        XCTAssertEqual(planarFrames, 1)
        let large = try sample(bytes: [UInt8](repeating: 0, count: 4096 * 2), bits: 16,
            flags: kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked, rate: 48000, channels: 1)
        var chunks = 0
        XCTAssertTrue(converter.convert(large) { _, frames, _, _, host, _ in
            XCTAssertEqual(frames, 1024)
            XCTAssertEqual(host, 1_000_000_000 + UInt64(chunks * 1024) * 1_000_000_000 / 48000)
            chunks += 1
        })
        XCTAssertEqual(chunks, 4)
    }
    func testSigned24AndBigEndianFloat() throws {
        let converter = OinkReplayPCM()
        let signed = try sample(bytes: [0x80, 0, 0], bits: 24,
            flags: kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsBigEndian | kAudioFormatFlagIsPacked,
            channels: 1)
        XCTAssertTrue(converter.convert(signed) { p, _, _, _, _, _ in XCTAssertEqual(p[0], -1) })
        let floating = try sample(bytes: [0x3f, 0, 0, 0], bits: 32,
            flags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsBigEndian | kAudioFormatFlagIsPacked, channels: 1)
        XCTAssertTrue(converter.convert(floating) { p, _, _, _, _, _ in XCTAssertEqual(p[0], 0.5) })
    }
    func testPreviousBroadcastPacketsAreDiscarded() throws {
        try withPipe { writer, reader in
            XCTAssertTrue(writer.claimBroadcast())
            let pcm: [Float] = [0.5]
            pcm.withUnsafeBufferPointer { writer.writePCM($0.baseAddress!, frames: 1, rate: 48000, channels: 1, hostNS: 0, discontinuity: false) }
            writer.endBroadcast()
            XCTAssertTrue(writer.claimBroadcast())
            var count = 0
            reader.drain { _, _, _, _, _, _ in count += 1 }
            XCTAssertEqual(count, 0)
        }
    }

    func testRingWrapsWithoutReordering() throws {
        try withPipe { writer, reader in
            XCTAssertTrue(writer.claimBroadcast())
            for index in 0..<1000 {
                let value: [Float] = [Float(index)]
                value.withUnsafeBufferPointer { writer.writePCM($0.baseAddress!, frames: 1, rate: 48000, channels: 1, hostNS: 0, discontinuity: false) }
                var count = 0
                reader.drain { p, _, _, _, _, _ in
                    XCTAssertEqual(p[0], Float(index)); count += 1
                }
                XCTAssertEqual(count, 1)
            }
        }
    }

    func testPresenceProbesDoNotStealAppClaims() throws {
        try withPipe { owner, observer in
            let probes = DispatchGroup()
            probes.enter()
            DispatchQueue.global().async {
                for _ in 0..<2000 { _ = observer.appRequested }
                probes.leave()
            }
            for _ in 0..<1000 {
                XCTAssertTrue(owner.claimApp())
                owner.releaseApp()
            }
            probes.wait()
        }
    }

}
