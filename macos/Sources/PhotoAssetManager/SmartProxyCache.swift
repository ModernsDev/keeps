import CoreImage
import CryptoKit
import Foundation
import KeepsAPI

actor SmartProxyCache {
    static let shared = SmartProxyCache()
    static let edge = 2560
    private let root: URL
    private let session: URLSession
    private let byteLimit: Int64
    private struct Pending {
        var id = UUID()
        var task: Task<URL, Error>
        var users: Set<UUID>
    }
    private var pending: [String: Pending] = [:]
    private var reading: [String: Int] = [:]
    private var lastTrim = Date.distantPast

    init(root: URL = URL.cachesDirectory.appendingPathComponent("Keeps/SmartProxies"),
         session: URLSession = KeepsClient.apiSession, byteLimit: Int64 = 8 * 1024 * 1024 * 1024) {
        self.root = root
        self.session = session
        self.byteLimit = byteLimit
    }

    nonisolated static func key(_ values: [String]) -> String {
        SHA256.hash(data: Data(values.map { "\($0.utf8.count):\($0)" }.joined().utf8))
            .map { String(format: "%02x", $0) }.joined()
    }

    nonisolated static func window(assets: [KeepsAsset], selected: UUID, radius: Int = 3) -> [KeepsAsset] {
        guard let index = assets.firstIndex(where: { $0.id == selected }) else { return [] }
        var indices = [index]
        if radius > 0 {
            for distance in 1...radius {
                if index + distance < assets.count { indices.append(index + distance) }
                if index >= distance { indices.append(index - distance) }
            }
        }
        return indices.map { assets[$0] }.filter { !$0.isVideo && !$0.trashed }
    }

    private func directory(_ configuration: KeepsConfiguration) throws -> URL {
        let directory = root.appendingPathComponent(Self.key([
            configuration.baseURL.absoluteString, configuration.libraryID, configuration.accessCredential ?? ""
        ]))
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true,
                                              attributes: [.posixPermissions: 0o700])
        return directory
    }

    func negative(assetID: UUID, state: KeepsEditState, configuration: KeepsConfiguration) async throws -> URL {
        guard let hash = state.negativeContentHash, let filename = state.sourceFilename else {
            throw ExposureJobError.missingNegative
        }
        let directory = try directory(configuration)
        let target = directory.appendingPathComponent(Self.key([assetID.uuidString, hash]))
            .appendingPathExtension(URL(fileURLWithPath: filename).pathExtension)
        return try await cached(target: target) { [session] in
            let temporary = directory.appendingPathComponent(UUID().uuidString).appendingPathExtension("download")
            defer { try? FileManager.default.removeItem(at: temporary) }
            try await KeepsClient(configuration: configuration, session: session)
                .downloadNegative(assetID: assetID, contentHash: hash, to: temporary)
            let info = try await AIEditingImages.inspect(temporary, image: false)
            guard info.hash == (state.sourceFileHash ?? hash) else { throw ExposureJobError.sourceChanged }
            try Task.checkCancellation()
            try FileManager.default.moveItem(at: temporary, to: target)
            return target
        }
    }

    func standard(asset: KeepsAsset, configuration: KeepsConfiguration) async throws -> URL {
        guard let standard = asset.standard else {
            let state = try await KeepsClient(configuration: configuration, session: session).editState(assetID: asset.id)
            return try await negative(assetID: asset.id, state: state, configuration: configuration)
        }
        let cache = PreviewCache.standards
        _ = try await cache.prefetch(assetID: asset.id, preview: standard, configuration: configuration, skipWhenBusy: false)
        let key = PreviewCache.key(assetID: asset.id, preview: standard, configuration: configuration, role: .standard)
        guard let url = try await cache.cachedFileURL(forKey: key) else { throw GeometryJobError.missingRender }
        return url
    }

    func proxy(source: URL, identity: String, configuration: KeepsConfiguration) async throws -> URL {
        reading[source.path, default: 0] += 1
        defer {
            reading[source.path]! -= 1
            if reading[source.path] == 0 { reading[source.path] = nil }
        }
        let target = try directory(configuration).appendingPathComponent(Self.key(["linear-v1", identity])).appendingPathExtension("tiff")
        return try await cached(target: target) {
            let work = Task.detached(priority: .utility) {
                let input = try Self.input(source)
                let scale = min(1, CGFloat(Self.edge) / max(input.extent.width, input.extent.height))
                let reduced = input.transformed(by: CGAffineTransform(scaleX: scale, y: scale))
                let pending = target.deletingLastPathComponent().appendingPathComponent(UUID().uuidString + ".tiff")
                defer { try? FileManager.default.removeItem(at: pending) }
                try Task.checkCancellation()
                try CIContext().writeTIFFRepresentation(of: reduced, to: pending, format: .RGBAh,
                    colorSpace: CGColorSpace(name: CGColorSpace.extendedLinearSRGB)!, options: [:])
                try Task.checkCancellation()
                try FileManager.default.moveItem(at: pending, to: target)
                return target
            }
            return try await withTaskCancellationHandler { try await work.value } onCancel: { work.cancel() }
        }
    }

    func display(asset: KeepsAsset, configuration: KeepsConfiguration, exposureEV: Double = 0) async throws -> CGImage {
        let source = try await standard(asset: asset, configuration: configuration)
        let identity = asset.standard.map {
            PreviewCache.key(assetID: asset.id, preview: $0, configuration: configuration, role: .standard)
        } ?? asset.id.uuidString + "|" + source.lastPathComponent
        let proxy = try await proxy(source: source, identity: identity, configuration: configuration)
        return try render(proxy: proxy, exposureEV: exposureEV)
    }

    func render(proxy: URL, exposureEV: Double) throws -> CGImage {
        try Task.checkCancellation()
        let input = try Self.input(proxy)
        let output = input.applyingFilter("CIExposureAdjust", parameters: [kCIInputEVKey: exposureEV])
        guard let image = CIContext().createCGImage(output, from: output.extent) else { throw ExposureRenderError.invalidImage }
        return image
    }

    nonisolated static func input(_ url: URL) throws -> CIImage {
        let raw: Set<String> = ["arw", "3fr", "dng", "cr2", "cr3", "nef", "nrw", "orf", "raf", "rw2", "pef", "srw"]
        let image: CIImage?
        if raw.contains(url.pathExtension.lowercased()) {
            image = CIRAWFilter(imageURL: url)?.outputImage
        } else {
            image = CIImage(contentsOf: url, options: [.applyOrientationProperty: true])
        }
        guard let image, !image.extent.isInfinite, !image.extent.isNull,
              image.extent.width > 0, image.extent.height > 0 else {
            throw ExposureRenderError.unreadableSource(url)
        }
        return image
    }

    private func cached(target: URL, load: @escaping @Sendable () async throws -> URL) async throws -> URL {
        try Task.checkCancellation()
        if FileManager.default.fileExists(atPath: target.path) {
            try FileManager.default.setAttributes([.modificationDate: Date()], ofItemAtPath: target.path)
            return target
        }
        let user = UUID()
        let key = target.path
        if let previous = pending[key], previous.task.isCancelled {
            _ = await previous.task.result
            if pending[key]?.id == previous.id { pending[key] = nil }
            return try await cached(target: target, load: load)
        }
        let task: Task<URL, Error>
        if let existing = pending[key] {
            task = existing.task
            pending[key]!.users.insert(user)
        } else {
            task = Task { try await load() }
            pending[key] = Pending(task: task, users: [user])
        }
        defer { release(key, user: user) }
        let result = try await withTaskCancellationHandler {
            try await task.value
        } onCancel: {
            Task { await self.release(key, user: user) }
        }
        try Task.checkCancellation()
        if Date().timeIntervalSince(lastTrim) >= 60 {
            do { try trim() }
            catch { NSLog("Smart proxy cache cleanup failed: %@", String(reflecting: error)) }
        }
        return result
    }

    private func release(_ key: String, user: UUID) {
        guard pending[key]?.users.remove(user) != nil else { return }
        if let value = pending[key], value.users.isEmpty {
            value.task.cancel()
            Task {
                _ = await value.task.result
                if pending[key]?.id == value.id { pending[key] = nil }
            }
        }
    }

    func prefetch(assets: [KeepsAsset], configuration: KeepsConfiguration) async {
        await withTaskGroup(of: Void.self) { group in
            group.addTask { await self.prefetchSources(assets: assets, configuration: configuration) }
            for asset in assets {
                guard !Task.isCancelled else { group.cancelAll(); return }
                do { _ = try await display(asset: asset, configuration: configuration) }
                catch {
                    guard !Task.isCancelled else { group.cancelAll(); return }
                    NSLog("Smart proxy preparation failed for %@: %@", asset.id.uuidString, String(reflecting: error))
                }
            }
        }
        do { try trim() }
        catch { NSLog("Smart proxy cache cleanup failed: %@", String(reflecting: error)) }
    }

    func trim() throws {
        lastTrim = Date()
        guard let entries = FileManager.default.enumerator(at: root,
            includingPropertiesForKeys: [.isRegularFileKey, .fileSizeKey, .contentModificationDateKey]) else { return }
        var files: [(URL, Int64, Date)] = []
        for case let url as URL in entries {
            let values = try url.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey, .contentModificationDateKey])
            guard values.isRegularFile == true, url.pathExtension != "download" else { continue }
            files.append((url, Int64(values.fileSize ?? 0), values.contentModificationDate ?? .distantPast))
        }
        var size = files.reduce(Int64(0)) { $0 + $1.1 }
        for (url, bytes, date) in files.sorted(by: { $0.2 < $1.2 }) {
            guard size > byteLimit else { break }
            guard date < Date().addingTimeInterval(-120), pending[url.path] == nil, reading[url.path] == nil else { continue }
            try FileManager.default.removeItem(at: url)
            size -= bytes
        }
    }

    private func prefetchSources(assets: [KeepsAsset], configuration: KeepsConfiguration) async {
        for asset in assets {
            guard !Task.isCancelled else { return }
            do {
                let state = try await KeepsClient(configuration: configuration, session: session).editState(assetID: asset.id)
                try Task.checkCancellation()
                _ = try await negative(assetID: asset.id, state: state, configuration: configuration)
            } catch {
                guard !Task.isCancelled else { return }
                NSLog("Smart proxy preparation failed for %@: %@", asset.id.uuidString, String(reflecting: error))
            }
        }
    }
}
