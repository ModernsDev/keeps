import CoreImage
import Foundation
import KeepsAPI
import Testing
@testable import PhotoAssetManager

@Suite(.serialized) @MainActor struct GeometryTaskStoreTests {
    @Test func proxyExposureCommitsFullSizeWithReplayableXMP() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let id = UUID()
        let state = GeometryProtocol.State()
        state.recipe = KeepsEditRecipe(recipeJSON: #"{"exposureEV":0.5}"#,
            xmp: #"<rdf:Description darktable:xmp_version="5"/>"#)
        GeometryProtocol.state = state
        let cache = try await sourceCache(root: root, assetID: id)
        let store = GeometryTaskStore(root: root, session: session())
        store.retryInterval = .milliseconds(10)
        store.configure(configuration)
        let source = try await store.prepareCrop(assetID: id, cache: cache)
        try store.exposure(session: source, ev: -0.75)
        try await wait { store.tasks.first?.phase == .completed || store.tasks.first?.failed == true }
        #expect(store.tasks.first?.phase == .completed)
        let commit = try #require(state.commit)
        #expect(commit.recipe == state.recipe)
        #expect(commit.outputs.first(where: { $0.role == "standard" })?.width == 64)
        #expect(commit.outputs.first(where: { $0.role == "standard" })?.height == 32)
        let edit = try PhotoEditXMP(xml: #require(commit.instructionsXMP))
        #expect(edit.operation == .displayExposure && edit.exposureEV == -0.75)
        #expect(edit.sourceVersion == source.sourceVersion)
    }

    @Test(arguments: [false, true], [[1], [-1], [2], [-1, -1, -1, -1, -1, 1]])
    func recropThroughRotationsRestoresFullSource(preserved: Bool, turns: [Int]) async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let assetID = UUID()
        let state = GeometryProtocol.State()
        GeometryProtocol.state = state
        let cache = try await sourceCache(root: root, assetID: assetID)
        let initialStore = GeometryTaskStore(root: root, session: session())
        initialStore.configure(configuration)
        let initial = try await initialStore.prepareCrop(assetID: assetID, cache: cache)
        var crop = GeometryTask(id: UUID(), assetID: assetID, baseURL: configuration.baseURL.absoluteString,
            libraryID: configuration.libraryID, operation: .crop,
            requestedCrop: CGRect(x: 0.125, y: 0.25, width: 0.5, height: 0.5))
        crop.sourceVersion = "existing-edit"
        crop.phase = .completed
        if preserved {
            crop.cropSourceID = initial.sourceID
            crop.cropSourceVersion = initial.sourceVersion
            crop.cropRatio = "原图比例"
        }
        var tasks = [crop]
        for turn in turns {
            var rotation = GeometryTask(id: UUID(), assetID: assetID, baseURL: configuration.baseURL.absoluteString,
                libraryID: configuration.libraryID, operation: .rotate, requestedQuarterTurns: turn)
            rotation.sourceVersion = "edit:\(tasks.last!.id.uuidString.lowercased()):hash"
            rotation.phase = .completed
            tasks.append(rotation)
        }
        state.lastRequestID = tasks.last!.id.uuidString.lowercased()
        try JSONEncoder().encode(tasks).write(to: root.appendingPathComponent("tasks.json"))
        let reopened = GeometryTaskStore(root: root, session: session())
        reopened.retryInterval = .milliseconds(10)
        reopened.configure(configuration)
        let restored = try await reopened.prepareCrop(assetID: assetID, cache: cache)
        let total = (turns.reduce(0, +) % 4 + 4) % 4
        let expected: CGRect = switch total {
        case 1: CGRect(x: 0.25, y: 0.375, width: 0.5, height: 0.5)
        case 2: CGRect(x: 0.375, y: 0.25, width: 0.5, height: 0.5)
        case 3: CGRect(x: 0.25, y: 0.125, width: 0.5, height: 0.5)
        default: crop.requestedCrop!
        }
        #expect(restored.selection == expected)
        #expect(restored.quarterTurns == total)
        #expect(restored.sourceURL == initial.sourceURL)
        #expect(try AIEditingImages.comparisonPreview(restored.sourceURL).width == 64)
        try reopened.crop(session: restored, selection: CGRect(x: 0, y: 0, width: 1, height: 1), ratio: "原图比例")
        try await wait { reopened.tasks.last?.phase == .completed || reopened.tasks.last?.failed == true }
        #expect(reopened.tasks.last?.phase == .completed)
        let output = try #require(state.commit?.outputs.first { $0.role == "standard" })
        #expect(output.width == (total % 2 == 0 ? 64 : 32))
        #expect(output.height == (total % 2 == 0 ? 32 : 64))
        let again = try await reopened.prepareCrop(assetID: assetID, cache: cache)
        #expect(again.quarterTurns == total)
        #expect(again.sourceURL == initial.sourceURL)
        #expect(again.selection == CGRect(x: 0, y: 0, width: 1, height: 1))
        #expect(!state.paths.contains("current-standard"))
    }

    @Test func recropAfterRestartRestoresFullSourceAndPreviousRectangle() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let assetID = UUID()
        let state = GeometryProtocol.State()
        state.recipe = KeepsEditRecipe(recipeJSON: #"{"modules":["exposure","colorbalancergb"]}"#,
            xmp: #"<rdf:Description darktable:xmp_version="5"/>"#, metadata: "{}")
        GeometryProtocol.state = state
        let cache = try await sourceCache(root: root, assetID: assetID)
        let store = GeometryTaskStore(root: root, session: session())
        store.retryInterval = .milliseconds(10)
        store.configure(configuration)
        let initial = try await store.prepareCrop(assetID: assetID, cache: cache)
        let rect = CGRect(x: 0.25, y: 0.25, width: 0.5, height: 0.5)
        try store.crop(session: initial, selection: rect, ratio: "原图比例")
        try await wait { store.tasks.last?.phase == .completed || store.tasks.last?.failed == true }
        #expect(store.tasks.last?.phase == .completed)
        #expect(state.commit?.outputs.first(where: { $0.role == "standard" })?.width == 32)
        #expect(state.commit?.outputs.first(where: { $0.role == "standard" })?.height == 16)
        try FileManager.default.removeItem(at: root.appendingPathComponent("cache"))
        let reopened = GeometryTaskStore(root: root, session: session())
        reopened.retryInterval = .milliseconds(10)
        reopened.configure(configuration)
        let restored = try await reopened.prepareCrop(assetID: assetID, cache: cache)
        #expect(restored.selection == rect)
        #expect(restored.ratio == "原图比例")
        #expect(restored.sourceURL == initial.sourceURL)
        #expect(restored.expectedRevision == 4)
        let image = try AIEditingImages.comparisonPreview(restored.sourceURL)
        #expect(image.width == 64 && image.height == 32)
        try reopened.crop(session: restored, selection: CGRect(x: 0, y: 0, width: 1, height: 1), ratio: "自由")
        try await wait { reopened.tasks.last?.phase == .completed || reopened.tasks.last?.failed == true }
        #expect(reopened.tasks.last?.phase == .completed)
        #expect(state.commit?.outputs.first(where: { $0.role == "standard" })?.width == 64)
        #expect(state.commit?.outputs.first(where: { $0.role == "standard" })?.height == 32)
        #expect(state.commit?.recipe == state.recipe)
        #expect(state.commit?.expectedRevision == 4)
        #expect(state.commitCount == 2)
        #expect(!state.paths.contains("current-standard"))
        try Data("[]".utf8).write(to: root.appendingPathComponent("tasks.json"))
        let withoutHistory = GeometryTaskStore(root: root, session: session())
        withoutHistory.configure(configuration)
        let fromXMP = try await withoutHistory.prepareCrop(assetID: assetID, cache: cache)
        #expect(fromXMP.sourceURL == initial.sourceURL)
        #expect(fromXMP.selection == CGRect(x: 0, y: 0, width: 1, height: 1))
    }

    @Test func legacyCropRestoresCachedFullSourceAndFailsWhenSourceIsMissing() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let assetID = UUID()
        let state = GeometryProtocol.State()
        GeometryProtocol.state = state
        let cache = try await sourceCache(root: root, assetID: assetID)
        var parent = GeometryTask(id: UUID(), assetID: assetID, baseURL: configuration.baseURL.absoluteString,
            libraryID: configuration.libraryID, operation: .crop, requestedCrop: CGRect(x: 0.1, y: 0.2, width: 0.8, height: 0.6))
        parent.sourceVersion = "existing-edit"
        parent.phase = .completed
        var child = GeometryTask(id: UUID(), assetID: assetID, baseURL: configuration.baseURL.absoluteString,
            libraryID: configuration.libraryID, operation: .crop, requestedCrop: CGRect(x: 0.25, y: 0.25, width: 0.5, height: 0.5))
        child.sourceVersion = "edit:\(parent.id.uuidString.lowercased()):hash"
        child.phase = .completed
        state.lastRequestID = child.id.uuidString.lowercased()
        try JSONEncoder().encode([parent, child]).write(to: root.appendingPathComponent("tasks.json"))
        let store = GeometryTaskStore(root: root, session: session())
        store.configure(configuration)
        let restored = try await store.prepareCrop(assetID: assetID, cache: cache)
        #expect(abs(restored.selection.minX - 0.3) < 0.000001)
        #expect(abs(restored.selection.minY - 0.35) < 0.000001)
        #expect(restored.selection.width == 0.4)
        #expect(restored.selection.height == 0.3)
        #expect(try AIEditingImages.comparisonPreview(restored.sourceURL).width == 64)
        try FileManager.default.removeItem(at: restored.sourceURL)
        try FileManager.default.removeItem(at: root.appendingPathComponent("cache"))
        await #expect(throws: (any Error).self) {
            _ = try await store.prepareCrop(assetID: assetID, cache: cache)
        }
        #expect(!state.paths.contains("current-standard"))
    }

    private func sourceCache(root: URL, assetID: UUID) async throws -> PreviewCache {
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let imageURL = root.appendingPathComponent("fixture.png")
        try CIContext().writePNGRepresentation(of: CIImage(color: .gray).cropped(to: CGRect(x: 0, y: 0, width: 64, height: 32)),
            to: imageURL, format: .RGBA8, colorSpace: CGColorSpace(name: CGColorSpace.sRGB)!, options: [:])
        let cache = PreviewCache(directory: root.appendingPathComponent("cache"), session: session(), role: .standard)
        let asset = try await KeepsClient(configuration: configuration, session: session()).asset(id: assetID)
        let preview = try #require(asset.standard)
        try await cache.importCachedFile(from: imageURL, key: PreviewCache.key(assetID: assetID, preview: preview, configuration: configuration, role: .standard))
        return cache
    }

    @Test func cropRejectsChangedRevisionBeforeFetchingImage() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let state = GeometryProtocol.State()
        GeometryProtocol.state = state
        let assetID = UUID()
        let cache = try await sourceCache(root: root, assetID: assetID)
        let store = GeometryTaskStore(root: root, session: session())
        store.configure(configuration)
        let prepared = try await store.prepareCrop(assetID: assetID, cache: cache)
        state.revision = 4
        state.paths = []
        try store.crop(session: prepared, selection: CGRect(x: 0, y: 0, width: 0.5, height: 1), ratio: "自由")
        try await wait { store.tasks.first?.failed == true }
        #expect(store.tasks.first?.phase == .preparing)
        #expect(state.paths == ["edit"])
        #expect(store.tasks.first?.error?.contains("conflict") == true)
    }

    @Test func persistedUploadingTaskPreservesRecipeAndRecoversLostCommitResponse() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let taskID = UUID()
        let assetID = UUID()
        let recipe = KeepsEditRecipe(recipeJSON: #"{"modules":["exposure","colorbalancergb"]}"#, xmp: #"<rdf:Description darktable:xmp_version="5"/>"#, metadata: #"{"existing":true}"#)
        let directory = root.appendingPathComponent(taskID.uuidString.lowercased())
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let files = ["standard", "thumbnail", "browse"].map { directory.appendingPathComponent($0 + ".png") }
        for file in files {
            try CIContext().writePNGRepresentation(of: CIImage(color: .gray).cropped(to: CGRect(x: 0, y: 0, width: 32, height: 16)),
                to: file, format: .RGBA8, colorSpace: CGColorSpace(name: CGColorSpace.sRGB)!, options: [:])
        }
        var task = GeometryTask(id: taskID, assetID: assetID, baseURL: configuration.baseURL.absoluteString,
                                libraryID: configuration.libraryID, operation: .rotate, requestedQuarterTurns: 1)
        task.exposureEV = nil
        task.recipe = recipe
        task.expectedRevision = 3
        task.sourceHash = "negative"
        task.phase = .uploading
        task.render = GeometryRender(width: 32, height: 16, standard: files[0], thumbnail: files[1], browse: files[2])
        try JSONEncoder().encode([task]).write(to: root.appendingPathComponent("tasks.json"))
        let state = GeometryProtocol.State()
        GeometryProtocol.state = state
        let store = GeometryTaskStore(root: root, session: session())
        store.retryInterval = .milliseconds(10)
        #expect(store.tasks.first?.recipe == recipe)
        store.configure(configuration)
        try await wait { store.tasks.first?.phase == .completed }
        let committed = try #require(state.commit)
        #expect(committed.recipe == recipe)
        #expect(committed.exposureEV == nil)
        #expect(committed.negativeContentHash == "negative")
        #expect(committed.expectedRevision == 3)
        #expect(committed.requestID == taskID.uuidString.lowercased())
        #expect(committed.algorithmVersion == "geometry-v1")
        #expect(Set(committed.outputs.map(\.role)) == Set(["standard", "thumbnail", "browse"]))
        #expect(state.commitCount == 1)
        #expect(store.completionRevision == 1)
        let reopened = GeometryTaskStore(root: root, session: session())
        #expect(reopened.tasks.first?.phase == .completed)
    }

    @Test func fullRotationWithAvailableNegativePreservesRecipe() async throws {
        try await fullRotation(edited: true)
    }

    @Test func uneditedRotationCommitsZeroExposure() async throws {
        try await fullRotation(edited: false)
    }

    @Test func unavailableNegativeFailsBeforeDownloadingStandard() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let state = GeometryProtocol.State()
        state.sourceAvailable = false
        GeometryProtocol.state = state
        let store = GeometryTaskStore(root: root, session: session())
        store.configure(configuration)
        store.rotate(assetID: UUID(), quarterTurns: 1)
        try await wait { store.tasks.first?.failed == true }
        #expect(state.paths == ["edit"])
        #expect(store.tasks.first?.phase == .preparing)
        #expect(state.commit == nil)
    }

    private func fullRotation(edited: Bool) async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let imageURL = root.appendingPathComponent("fixture.png")
        try CIContext().writePNGRepresentation(of: CIImage(color: .gray).cropped(to: CGRect(x: 0, y: 0, width: 64, height: 32)),
            to: imageURL, format: .RGBA8, colorSpace: CGColorSpace(name: CGColorSpace.sRGB)!, options: [:])
        let state = GeometryProtocol.State()
        state.sourceData = try Data(contentsOf: imageURL)
        state.recipe = edited ? KeepsEditRecipe(recipeJSON: #"{"modules":["exposure","colorbalancergb"]}"#,
            xmp: #"<rdf:Description darktable:xmp_version="5"/>"#, metadata: #"{"existing":true}"#) : nil
        state.exposureEV = nil
        GeometryProtocol.state = state
        let store = GeometryTaskStore(root: root, session: session())
        store.retryInterval = .milliseconds(10)
        store.configure(configuration)
        store.rotate(assetID: UUID(), quarterTurns: 1)
        try await wait { store.tasks.first?.phase == .completed || store.tasks.first?.failed == true }
        #expect(store.tasks.first?.phase == .completed)
        let committed = try #require(state.commit)
        #expect(committed.recipe == state.recipe)
        #expect(committed.exposureEV == (edited ? nil : 0))
        #expect(state.paths.contains("current-standard"))
        #expect(committed.outputs.first(where: { $0.role == "standard" })?.width == 32)
        #expect(committed.outputs.first(where: { $0.role == "standard" })?.height == 64)
        #expect(state.uploads == 3)
    }

    private var configuration: KeepsConfiguration {
        KeepsConfiguration(baseURL: URL(string: "https://geometry.test")!, libraryID: "test")
    }
    private func session() -> URLSession {
        let config = URLSessionConfiguration.ephemeral
        config.protocolClasses = [GeometryProtocol.self]
        return URLSession(configuration: config)
    }
    private func wait(_ predicate: () -> Bool) async throws {
        for _ in 0..<300 {
            if predicate() { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(predicate())
    }
}

private final class GeometryProtocol: URLProtocol, @unchecked Sendable {
    final class State: @unchecked Sendable {
        let lock = NSLock()
        var paths: [String] = []
        var commit: KeepsEditCommit?
        var commitCount = 0
        var revision: Int64 = 3
        var lastRequestID: String?
        var sourceData: Data?
        var recipe: KeepsEditRecipe?
        var uploads = 0
        var sourceAvailable = true
        var exposureEV: Double? = 0.75
    }
    nonisolated(unsafe) static var state: State!
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        let state = Self.state!
        state.lock.lock()
        defer { state.lock.unlock() }
        state.paths.append(request.url!.lastPathComponent)
        do {
            let data: Data
            if request.httpMethod == "PUT", request.url!.lastPathComponent == "edit" {
                var body = request.httpBody ?? Data()
                if let stream = request.httpBodyStream {
                    stream.open(); defer { stream.close() }
                    var buffer = [UInt8](repeating: 0, count: 8192)
                    while stream.hasBytesAvailable {
                        let count = stream.read(&buffer, maxLength: buffer.count)
                        if count <= 0 { break }
                        body.append(contentsOf: buffer.prefix(count))
                    }
                }
                let commit = try JSONDecoder().decode(KeepsEditCommit.self, from: body)
                if let recipe = commit.recipe {
                    guard commit.exposureEV == nil, recipe.engine == "darktable", recipe.engineVersion == "5.6.2",
                          let json = recipe.recipeJSON.data(using: .utf8),
                          (try? JSONSerialization.jsonObject(with: json)) is [String: Any],
                          recipe.xmp.contains("darktable:xmp_version") else {
                        throw KeepsAPIError.http(422, "Invalid recipe/exposure contract")
                    }
                } else {
                    guard let ev = commit.exposureEV, ev.isFinite, (-2...2).contains(ev) else {
                        throw KeepsAPIError.http(422, "Exposure required without recipe")
                    }
                }
                guard state.sourceAvailable else { throw KeepsAPIError.http(409, "Negative unavailable") }
                state.commit = commit
                state.commitCount += 1
                client?.urlProtocol(self, didFailWithError: URLError(.networkConnectionLost))
                return
            } else if request.url!.lastPathComponent == "edit" {
                data = try JSONEncoder().encode(KeepsEditState(negativeContentHash: "negative", revision: state.revision + Int64(state.commitCount),
                    hasEdit: state.recipe != nil || state.exposureEV != nil, exposureEV: state.recipe == nil ? state.exposureEV : nil, sourceAvailable: state.sourceAvailable, lastRequestID: state.commit?.requestID ?? state.lastRequestID, recipe: state.recipe, instructionsXMP: state.commit?.instructionsXMP))
            } else if request.url!.lastPathComponent == "uploads" {
                data = try JSONEncoder().encode(KeepsEditUploadSession(requestID: "request", objects: ["standard", "thumbnail", "browse"].map {
                    KeepsEditUploadTarget(role: $0, objectRef: KeepsEditObjectRef(bucket: "media", key: $0), uploadURL: URL(string: "https://geometry.test/" + $0)!)
                }))
            } else if request.url!.lastPathComponent == "current-standard" {
                data = state.sourceData ?? Data()
            } else if request.httpMethod == "GET", let id = UUID(uuidString: request.url!.lastPathComponent) {
                let output = state.commit?.outputs.first(where: { $0.role == "standard" })
                let version = state.commit.map { "edit:\($0.requestID):hash" } ?? "existing-edit"
                data = Data("""
                {"id":"\(id)","cameraMake":"","cameraModel":"","lensModel":"","originalFilename":"original.ARW","contentFingerprint":"hash","metadataFingerprint":"meta","rating":0,"flagState":"unflagged","tags":[],"createdAt":"2024-01-01","updatedAt":"2024-01-01","trashed":false,"standard":{"downloadURL":"https://geometry.test/current-standard","width":\(output?.width ?? 64),"height":\(output?.height ?? 32),"version":"\(version)"}}
                """.utf8)
            } else {
                if request.httpMethod == "PUT" { state.uploads += 1 }
                data = Data()
            }
            client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: 200,
                httpVersion: nil, headerFields: nil)!, cacheStoragePolicy: .notAllowed)
            client?.urlProtocol(self, didLoad: data)
            client?.urlProtocolDidFinishLoading(self)
        } catch { client?.urlProtocol(self, didFailWithError: error) }
    }
    override func stopLoading() {}
}
