import CoreGraphics
import Foundation
import ImageIO
import KeepsAPI
import Testing
import UniformTypeIdentifiers
@testable import PhotoAssetManager

struct ImportRendererTests {
    @Test func rendersRealHEICWithOrientedDimensions() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let input = root.appendingPathComponent("source.jpg")
        let context = try #require(CGContext(data: nil, width: 800, height: 400, bitsPerComponent: 8, bytesPerRow: 3200, space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue))
        context.setFillColor(CGColor(red: 0.5, green: 0.2, blue: 0.1, alpha: 1))
        context.fill(CGRect(x: 0, y: 0, width: 800, height: 400))
        let destination = try #require(CGImageDestinationCreateWithURL(input as CFURL, UTType.jpeg.identifier as CFString, 1, nil))
        CGImageDestinationAddImage(destination, try #require(context.makeImage()), [kCGImagePropertyOrientation: 6, kCGImagePropertyExifDictionary: [kCGImagePropertyExifDateTimeOriginal: "2026:10:11 10:20:30"], kCGImagePropertyTIFFDictionary: [kCGImagePropertyTIFFMake: "Camera Make", kCGImagePropertyTIFFModel: "Camera Model"]] as CFDictionary)
        #expect(CGImageDestinationFinalize(destination))
        let task = try JSONDecoder().decode(KeepsWorkerTask.self, from: Data("""
        {"taskID":"test","inputHash":"unused","filename":"source.jpg","generateStandard":true,"generateBrowse":true,"thumbnailEdge":512,"thumbnailQuality":85}
        """.utf8))
        try ImportRenderer.render(input: input, root: root, task: task)
        for (name, width, height) in [("thumbnail", 256, 512), ("browse", 32, 64), ("standard", 400, 800)] {
            let source = try #require(CGImageSourceCreateWithURL(root.appendingPathComponent(name + ".heic") as CFURL, nil))
            #expect(CGImageSourceGetType(source) as String? == UTType.heic.identifier)
            let image = try #require(CGImageSourceCreateImageAtIndex(source, 0, nil))
            #expect(image.width == width)
            #expect(image.height == height)
            #expect(image.colorSpace != nil)
            if name == "standard" {
                let properties = try #require(CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [String: Any])
                let exif = try #require(properties[kCGImagePropertyExifDictionary as String] as? [String: Any])
                let tiff = try #require(properties[kCGImagePropertyTIFFDictionary as String] as? [String: Any])
                #expect(exif[kCGImagePropertyExifDateTimeOriginal as String] as? String == "2026:10:11 10:20:30")
                #expect(tiff[kCGImagePropertyTIFFMake as String] as? String == "Camera Make")
                #expect(tiff[kCGImagePropertyTIFFModel as String] as? String == "Camera Model")
            }
        }
    }
}

struct ImportRepairTests {
    @Test func verifiesCameraCopyAndRejectsUndecodableSource() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let staging = root.appendingPathComponent("staging")
        try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let camera = root.appendingPathComponent("photo.jpg")
        let copy = staging.appendingPathComponent("photo.jpg")
        let context = try #require(CGContext(data: nil, width: 64, height: 32, bitsPerComponent: 8, bytesPerRow: 256, space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue))
        context.setFillColor(CGColor(red: 0.5, green: 0.2, blue: 0.1, alpha: 1))
        context.fill(CGRect(x: 0, y: 0, width: 64, height: 32))
        let destination = try #require(CGImageDestinationCreateWithURL(camera as CFURL, UTType.jpeg.identifier as CFString, 1, nil))
        CGImageDestinationAddImage(destination, try #require(context.makeImage()), nil)
        #expect(CGImageDestinationFinalize(destination))
        let task = try JSONDecoder().decode(KeepsWorkerTask.self, from: Data("""
        {"taskID":"test","inputHash":"bad","filename":"photo.jpg","generateStandard":false,"generateBrowse":true,"thumbnailEdge":512,"thumbnailQuality":85}
        """.utf8))
        let original = try Data(contentsOf: camera)
        let (size, hash) = try ImportSource.hash(camera)
        #expect(try ImportRenderer.repairCopy(source: camera, size: size, nasHash: hash, copy: copy, task: task) == nil)
        #expect(!FileManager.default.fileExists(atPath: copy.path))
        #expect(try ImportRenderer.repairCopy(source: camera, size: size, nasHash: "different", copy: copy, task: task) == hash)
        #expect(try Data(contentsOf: copy) == original)
        #expect(try Data(contentsOf: camera) == original)
        let badCamera = root.appendingPathComponent("bad.jpg")
        try Data("corrupt".utf8).write(to: badCamera)
        #expect(throws: (any Error).self) {
            try ImportRenderer.repairCopy(source: badCamera, size: 7, nasHash: "different", copy: staging.appendingPathComponent("bad.jpg"), task: task)
        }
    }

    @MainActor @Test func mediaErrorReportsItsOwnFile() {
        let client = KeepsClient(configuration: KeepsConfiguration(baseURL: URL(string: "http://localhost")!, libraryID: UUID().uuidString))
        let store = ImportStore(client: client)
        store.report(NSError(domain: "KeepsImportMedia", code: 1, userInfo: [NSFilePathErrorKey: "B0022822.HEIC", NSLocalizedDescriptionKey: "Mac 图片编码失败"]))
        #expect(store.errorMessage?.hasPrefix("文件：B0022822.HEIC\n") == true)
    }
}
