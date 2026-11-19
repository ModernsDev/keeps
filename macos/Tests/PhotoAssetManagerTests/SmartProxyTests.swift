import CoreImage
import CryptoKit
import Foundation
import KeepsAPI
import Testing
import os
@testable import PhotoAssetManager

struct SmartProxyTests {
    @Test func evictionOnlyRemovesOldDisposableCacheFiles() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let cacheRoot = root.appendingPathComponent("cache")
        try FileManager.default.createDirectory(at: cacheRoot, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let source = root.appendingPathComponent("original")
        let old = cacheRoot.appendingPathComponent("old.tiff")
        let current = cacheRoot.appendingPathComponent("current.tiff")
        for file in [source, old, current] { try Data(repeating: 1, count: 16).write(to: file) }
        try FileManager.default.setAttributes([.modificationDate: Date.distantPast], ofItemAtPath: old.path)
        let cache = SmartProxyCache(root: cacheRoot, byteLimit: 16)
        try await cache.trim()
        #expect(!FileManager.default.fileExists(atPath: old.path))
        #expect(try Data(contentsOf: source).count == 16)
        #expect(try Data(contentsOf: current).count == 16)
    }

    @Test func proxyPreservesLinearHighlightsAndMapsCropToFullResolution() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let colorSpace = CGColorSpace(name: CGColorSpace.extendedLinearSRGB)!
        let input = CIImage(color: CIColor(red: 2, green: 0.25, blue: 0.0625, colorSpace: colorSpace)!)
            .cropped(to: CGRect(x: 0, y: 0, width: 3200, height: 1600))
        let source = root.appendingPathComponent("source.tiff")
        try CIContext().writeTIFFRepresentation(of: input, to: source, format: .RGBAh, colorSpace: colorSpace, options: [:])
        let before = try await AIEditingImages.inspect(source, image: false).hash
        let configuration = KeepsConfiguration(baseURL: URL(string: "https://proxy.test")!, libraryID: "test")
        let cache = SmartProxyCache(root: root.appendingPathComponent("cache"))
        let proxy = try await cache.proxy(source: source, identity: "version1", configuration: configuration)
        let low = try SmartProxyCache.input(proxy)
        #expect(low.extent.width == 2560 && low.extent.height == 1280)
        var pixel = [Float](repeating: 0, count: 4)
        CIContext().render(low, toBitmap: &pixel, rowBytes: 16, bounds: CGRect(x: 0, y: 0, width: 1, height: 1),
                           format: .RGBAf, colorSpace: colorSpace)
        #expect(abs(pixel[0] - 2) < 0.01)
        let id = UUID()
        let edit = PhotoEditXMP(assetID: id, rootID: id.uuidString, versionID: UUID(),
            negativeHash: before, sourceVersion: "version1", operation: .geometry, quarterTurns: 1,
            crop: CGRect(x: 0.125, y: 0.25, width: 0.5, height: 0.5), exposureEV: -1)
        let restored = try PhotoEditXMP(xml: edit.xml())
        #expect(restored == edit)
        let preview = try GeometryRenderer.geometry(low, quarterTurns: restored.quarterTurns!, crop: restored.crop)
        let output = try await GeometryRenderer().render(sourceURL: source, outputDirectory: root.appendingPathComponent("output"),
            quarterTurns: restored.quarterTurns!, crop: restored.crop, exposureEV: restored.exposureEV!)
        #expect(preview.extent.width == 640 && preview.extent.height == 1280)
        #expect(output.width == 800 && output.height == 1600)
        #expect(try await AIEditingImages.inspect(source, image: false).hash == before)
        #expect(try await cache.proxy(source: source, identity: "version1", configuration: configuration) == proxy)
        let changed = try await cache.proxy(source: source, identity: "version2", configuration: configuration)
        #expect(changed != proxy)
        var other = configuration
        other.accessCredential = "different-account"
        #expect(try await cache.proxy(source: source, identity: "version1", configuration: other) != proxy)
    }

    @Test func xmpRoundTripsColorRecipeAndRejectsForeignNamespace() throws {
        let id = UUID()
        let recipe = KeepsEditRecipe(recipeJSON: #"{"exposureEV":0.5,"note":"<&>"}"#,
            xmp: #"<x:xmpmeta xmlns:x="adobe:ns:meta/"><!-- & --></x:xmpmeta>"#)
        let edit = PhotoEditXMP(assetID: id, rootID: id.uuidString, versionID: UUID(),
            negativeHash: "source", sourceVersion: "source", operation: .color, recipe: recipe)
        let xml = try edit.xml()
        #expect(try PhotoEditXMP(xml: xml) == edit)
        #expect(throws: (any Error).self) {
            try PhotoEditXMP(xml: xml.replacingOccurrences(of: PhotoEditXMP.namespace, with: "https://invalid.test/"))
        }
    }

    @Test func bufferUsesCurrentThenNearestNeighborsWithoutWrapping() throws {
        let assets = try (0..<8).map { index in
            try JSONDecoder().decode(KeepsAsset.self, from: Data("""
            {"id":"\(UUID())","cameraMake":"","cameraModel":"","lensModel":"","originalFilename":"\(index).jpg","contentFingerprint":"hash","metadataFingerprint":"meta","rating":0,"flagState":"unflagged","tags":[],"createdAt":"2024-01-01","updatedAt":"2024-01-01","trashed":false}
            """.utf8))
        }
        #expect(SmartProxyCache.window(assets: assets, selected: assets[3].id).map(\.id) ==
            [3, 4, 2, 5, 1, 6, 0].map { assets[$0].id })
        #expect(SmartProxyCache.window(assets: assets, selected: assets[0].id).map(\.id) ==
            [0, 1, 2, 3].map { assets[$0].id })
        #expect(SmartProxyCache.window(assets: assets, selected: UUID()).isEmpty)
    }
}

@Suite(.serialized) struct SmartProxyDownloadTests {
    @Test func sharedDownloadSurvivesOneCancelledReaderAndIsReusedAfterRestart() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let sessionConfiguration = URLSessionConfiguration.ephemeral
        sessionConfiguration.protocolClasses = [ProxyDownloadProtocol.self]
        let session = URLSession(configuration: sessionConfiguration)
        defer { session.invalidateAndCancel() }
        ProxyDownloadProtocol.requests.withLock { $0 = 0 }
        let cache = SmartProxyCache(root: root, session: session)
        let configuration = KeepsConfiguration(baseURL: URL(string: "https://proxy.test")!, libraryID: "test")
        let hash = SHA256.hash(data: ProxyDownloadProtocol.bytes).map { String(format: "%02x", $0) }.joined()
        let state = KeepsEditState(negativeContentHash: hash, revision: 1, hasEdit: false,
            sourceFilename: "source.raw", sourceAvailable: true, sourceFileHash: hash)
        let id = UUID()
        let first = Task { try await cache.negative(assetID: id, state: state, configuration: configuration) }
        let second = Task { try await cache.negative(assetID: id, state: state, configuration: configuration) }
        try await Task.sleep(for: .milliseconds(50))
        first.cancel()
        let file = try await second.value
        _ = await first.result
        #expect(ProxyDownloadProtocol.requests.withLock { $0 } == 1)
        #expect(try Data(contentsOf: file) == ProxyDownloadProtocol.bytes)
        let reopened = SmartProxyCache(root: root, session: session)
        #expect(try await reopened.negative(assetID: id, state: state, configuration: configuration) == file)
        #expect(ProxyDownloadProtocol.requests.withLock { $0 } == 1)
    }
}

private final class ProxyDownloadProtocol: URLProtocol, @unchecked Sendable {
    static let bytes = Data(repeating: 37, count: 8192)
    static let requests = OSAllocatedUnfairLock(initialState: 0)
    private var responseTask: Task<Void, Never>?
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        Self.requests.withLock { $0 += 1 }
        responseTask = Task {
            do { try await Task.sleep(for: .milliseconds(200)) } catch { return }
            client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: 200,
                httpVersion: nil, headerFields: ["Content-Length": String(Self.bytes.count)])!, cacheStoragePolicy: .notAllowed)
            client?.urlProtocol(self, didLoad: Self.bytes)
            client?.urlProtocolDidFinishLoading(self)
        }
    }
    override func stopLoading() { responseTask?.cancel() }
}
