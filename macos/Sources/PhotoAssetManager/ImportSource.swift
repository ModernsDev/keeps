import CryptoKit
import Foundation

struct ImportSourceFile: Codable, Sendable {
    let url: URL
    let sourcePath: String
    let size: Int64
    let modifiedAt: Date
}

enum ImportSource {
    private static let photoExtensions: Set<String> = [
        "3fr", "ari", "arw", "bay", "cr2", "cr3", "crw", "dcr", "dng", "erf", "fff", "iiq", "k25",
        "kdc", "mef", "mos", "mrw", "nef", "nrw", "orf", "pef", "raf", "raw", "rw2", "rwl", "sr2",
        "srf", "srw", "heif", "heic", "hif", "jpg", "jpeg",
    ]

    static func scan(_ root: URL) throws -> [ImportSourceFile] {
        try Task.checkCancellation()
        let root = root.standardizedFileURL
        let values = try root.resourceValues(forKeys: [.isDirectoryKey, .isSymbolicLinkKey])
        guard values.isDirectory == true, values.isSymbolicLink != true else {
            throw ScanError.invalidDirectory(root.path)
        }
        var files: [ImportSourceFile] = []
        try scanDirectory(root, relativePath: "", files: &files)
        return files.sorted { $0.sourcePath < $1.sourcePath }
    }

    private static func scanDirectory(_ directory: URL, relativePath: String, files: inout [ImportSourceFile]) throws {
        try Task.checkCancellation()
        let keys: Set<URLResourceKey> = [.isDirectoryKey, .isRegularFileKey, .isSymbolicLinkKey, .isHiddenKey]
        let children = try FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: Array(keys))
        var regularFiles: [URL] = []
        for child in children.sorted(by: { $0.lastPathComponent < $1.lastPathComponent }) {
            try Task.checkCancellation()
            let values = try child.resourceValues(forKeys: keys)
            guard values.isSymbolicLink != true, values.isHidden != true,
                  !child.lastPathComponent.hasPrefix("."),
                  !["@eadir", "#recycle"].contains(child.lastPathComponent.lowercased()) else { continue }
            if values.isDirectory == true {
                try scanDirectory(child, relativePath: relativePath + child.lastPathComponent + "/", files: &files)
            } else if values.isRegularFile == true {
                regularFiles.append(child)
            }
        }
        let photos = regularFiles.filter { photoExtensions.contains($0.pathExtension.lowercased()) }
        let sidecarNames = Set(photos.flatMap {
            [$0.deletingPathExtension().lastPathComponent.lowercased() + ".xmp", $0.lastPathComponent.lowercased() + ".xmp"]
        })
        for file in regularFiles where photoExtensions.contains(file.pathExtension.lowercased()) || sidecarNames.contains(file.lastPathComponent.lowercased()) {
            let attributes = try FileManager.default.attributesOfItem(atPath: file.path)
            guard let size = attributes[.size] as? NSNumber,
                  let modifiedAt = attributes[.modificationDate] as? Date else {
                throw ScanError.changedDuringRead(file.path)
            }
            files.append(ImportSourceFile(url: file, sourcePath: relativePath + file.lastPathComponent,
                                          size: size.int64Value, modifiedAt: modifiedAt))
        }
    }

    static func groupKey(_ relativePath: String) -> String {
        var path = URL(fileURLWithPath: relativePath)
        if path.pathExtension.lowercased() == "xmp" {
            path.deletePathExtension()
            if photoExtensions.contains(path.pathExtension.lowercased()) { path.deletePathExtension() }
        } else {
            path.deletePathExtension()
        }
        return path.deletingLastPathComponent().path + "/" + path.lastPathComponent.lowercased()
    }

    static func sidecarPhotoID(_ url: URL) throws -> UUID? {
        guard url.pathExtension.lowercased() == "xmp" else { return nil }
        let data = try Data(contentsOf: url)
        let identifierPrefix = "https://keeps.photos/photo/"
        let legacyNamespace = "https://keeps.photos/ns/1.0/"
        guard data.range(of: Data(identifierPrefix.utf8)) != nil ||
                data.range(of: Data(legacyNamespace.utf8)) != nil else { return nil }
        let xml = try XMLDocument(data: data, options: [.nodeLoadExternalEntitiesNever])
        let identifiers = try xml.nodes(forXPath: "//*[local-name()='Identifier'] | //@*[local-name()='Identifier']")
        for identifier in identifiers where identifier.uri == "http://ns.adobe.com/xap/1.0/" {
            let nested = identifier is XMLElement ? try identifier.nodes(forXPath: ".//*[not(*)]") : []
            for node in nested.isEmpty ? [identifier] : nested {
                if let value = node.stringValue, value.hasPrefix(identifierPrefix),
                   let id = UUID(uuidString: String(value.dropFirst(identifierPrefix.count))) { return id }
            }
        }
        let legacy = try xml.nodes(forXPath: "//*[local-name()='PhotoID'] | //@*[local-name()='PhotoID']")
        return legacy.lazy.filter { $0.uri == legacyNamespace }
            .compactMap { $0.stringValue.flatMap(UUID.init(uuidString:)) }.first
    }

    static func hash(_ url: URL) throws -> (Int64, String) {
        try Task.checkCancellation()
        let before = try FileManager.default.attributesOfItem(atPath: url.path)
        let handle = try FileHandle(forReadingFrom: url)
        var hasher = SHA256()
        var bytesRead: Int64 = 0
        // FileHandle buffers can otherwise survive until the detached task finishes.
        while try autoreleasepool(invoking: { () throws -> Bool in
            try Task.checkCancellation()
            guard let chunk = try handle.read(upToCount: 1_048_576), !chunk.isEmpty else { return false }
            bytesRead += Int64(chunk.count)
            hasher.update(data: chunk)
            return true
        }) {}
        try handle.close()
        let after = try FileManager.default.attributesOfItem(atPath: url.path)
        guard let size = before[.size] as? NSNumber,
              let modified = before[.modificationDate] as? Date,
              size == after[.size] as? NSNumber,
              modified == after[.modificationDate] as? Date,
              bytesRead == size.int64Value else {
            throw ScanError.changedDuringRead(url.path)
        }
        return (bytesRead, hasher.finalize().map { String(format: "%02x", $0) }.joined())
    }

    private enum ScanError: LocalizedError {
        case invalidDirectory(String)
        case changedDuringRead(String)

        var errorDescription: String? {
            switch self {
            case .invalidDirectory(let path): "导入来源必须是实际文件夹：\(path)"
            case .changedDuringRead(let path): "读取期间文件发生变化，请重试：\(path)"
            }
        }
    }
}
