import SwiftUI
import CoreImage
import KeepsAPI

struct PhotoCropEditor: View {
    let asset: KeepsAsset
    let close: () -> Void
    @EnvironmentObject private var library: LibraryStore
    @EnvironmentObject private var geometryTasks: GeometryTaskStore
    @State private var image: CGImage?
    @State private var selection = CGRect(x: 0, y: 0, width: 1, height: 1)
    @State private var loadError: String?
    @State private var retry = 0
    @State private var session: PhotoCropSession?
    @State private var saveError: String?
    @State private var ratio: CropRatio = .original
    @State private var drag: CropDrag?

    private var unavailable: Bool {
        asset.trashed || library.isOperationBlocking || library.configuration == nil || geometryTasks.storageError != nil || geometryTasks.isPending(assetID: asset.id)
    }

    var body: some View {
        VStack(spacing: 0) {
            if let image {
                cropCanvas(image).padding(24)
            } else if let loadError {
                VStack {
                    WorkspaceErrorView(title: "无法载入裁剪照片", details: loadError)
                    Button("重新载入") { retry += 1 }
                }.frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ProgressView("正在载入照片…").frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            if let error = saveError ?? geometryTasks.storageError {
                WorkspaceErrorView(title: "无法保存裁剪", details: error).padding(12)
            }
            HStack(spacing: 12) {
                Picker("比例", selection: Binding(get: { ratio }, set: {
                    ratio = $0
                    selection = fittedSelection()
                })) {
                    ForEach(CropRatio.allCases, id: \.self) { value in
                        Text(value.rawValue).tag(value)
                    }
                }.frame(width: 170).disabled(image == nil)
                Button { selection = fittedSelection() } label: { Image(systemName: "arrow.counterclockwise") }
                    .help("重置裁剪范围").accessibilityLabel("重置裁剪范围").disabled(image == nil)
                Spacer()
                Button("取消", action: close).keyboardShortcut(.cancelAction)
                Button("应用裁剪") {
                    guard let session else { return }
                    do {
                        try geometryTasks.crop(session: session, selection: CGRect(
                            x: selection.minX, y: max(0, 1 - selection.maxY),
                            width: selection.width, height: selection.height
                        ), ratio: ratio.rawValue)
                        close()
                    } catch {
                        saveError = String(reflecting: error) + "\n" + error.localizedDescription
                    }
                }.keyboardShortcut(.defaultAction)
                    .disabled(unavailable || image == nil || session == nil || selection.width <= 0 || selection.height <= 0)
            }.padding(.horizontal, 16).frame(height: 46).background(WorkspaceStyle.panel)
        }
            .task(id: "\(asset.id)|\(asset.standard?.version ?? "")|\(library.configuration?.baseURL.absoluteString ?? "")|\(library.configuration?.libraryID ?? "")|\(retry)") {
                await loadImage()
            }
    }

    private func cropCanvas(_ image: CGImage) -> some View {
        GeometryReader { geometry in
            let scale = min(geometry.size.width / CGFloat(image.width), geometry.size.height / CGFloat(image.height))
            let size = CGSize(width: CGFloat(image.width) * scale, height: CGFloat(image.height) * scale)
            ZStack {
                Color.black
                ZStack(alignment: .topLeading) {
                    Image(decorative: image, scale: 1).resizable().frame(width: size.width, height: size.height)
                    Path { path in
                        path.addRect(CGRect(origin: .zero, size: size))
                        path.addRect(CGRect(x: selection.minX * size.width, y: selection.minY * size.height,
                                            width: selection.width * size.width, height: selection.height * size.height))
                    }.fill(.black.opacity(0.55), style: FillStyle(eoFill: true))
                    Rectangle().stroke(.white, lineWidth: 1)
                        .frame(width: selection.width * size.width, height: selection.height * size.height)
                        .offset(x: selection.minX * size.width, y: selection.minY * size.height)
                    ForEach(0..<4, id: \.self) { corner in
                        let point = cornerPoint(corner, rect: selection)
                        Circle().fill(.white).frame(width: 10, height: 10)
                            .position(x: point.x * size.width, y: point.y * size.height)
                    }
                }
                .frame(width: size.width, height: size.height)
                .overlay {
                    CropPointerInput(enabled: !unavailable, changed: { start, end in
                        updateSelection(from: start, to: end, size: size)
                    }, ended: { drag = nil })
                }
                .accessibilityLabel("在照片上拖动选择裁剪范围")
            }
        }
    }

    private func updateSelection(from location: CGPoint, to destination: CGPoint, size: CGSize) {
        if drag == nil {
            let start = bounded(location, size: size)
            let corner = (0..<4).first { index in
                let point = cornerPoint(index, rect: selection)
                return hypot((point.x - start.x) * size.width, (point.y - start.y) * size.height) < 16
            }
            if let corner {
                drag = .resize(cornerPoint(3 - corner, rect: selection))
            } else if selection.contains(start), selection != CGRect(x: 0, y: 0, width: 1, height: 1) {
                drag = .move(start, selection)
            } else {
                drag = .resize(start)
            }
        }
        let end = bounded(destination, size: size)
        switch drag {
        case .resize(let anchor):
            let rect = rectangle(anchor: anchor, end: end, size: size)
            if rect.width * size.width >= 2 && rect.height * size.height >= 2 { selection = rect }
        case .move(let start, let original):
            selection.origin.x = min(1 - original.width, max(0, original.minX + end.x - start.x))
            selection.origin.y = min(1 - original.height, max(0, original.minY + end.y - start.y))
        case nil: break
        }
    }

    private func cornerPoint(_ index: Int, rect: CGRect) -> CGPoint {
        CGPoint(x: index % 2 == 0 ? rect.minX : rect.maxX, y: index < 2 ? rect.minY : rect.maxY)
    }

    private func aspectRatio(portrait: Bool? = nil) -> CGFloat? {
        guard let image else { return nil }
        let portrait = portrait ?? (selection.height * CGFloat(image.height) > selection.width * CGFloat(image.width))
        let value: CGFloat
        switch ratio {
        case .original: value = CGFloat(max(image.width, image.height)) / CGFloat(min(image.width, image.height))
        case .free: return nil
        case .square: value = 1
        case .fourThree: value = 4 / 3
        case .threeTwo: value = 3 / 2
        case .sixteenNine: value = 16 / 9
        }
        return portrait ? 1 / value : value
    }

    private func fittedSelection() -> CGRect {
        guard let image, let aspect = aspectRatio() else { return CGRect(x: 0, y: 0, width: 1, height: 1) }
        let normalized = aspect * CGFloat(image.height) / CGFloat(image.width)
        let width = min(1, normalized)
        let height = min(1, 1 / normalized)
        return CGRect(x: (1 - width) / 2, y: (1 - height) / 2, width: width, height: height)
    }

    private func rectangle(anchor: CGPoint, end: CGPoint, size: CGSize) -> CGRect {
        let dx = end.x - anchor.x
        let dy = end.y - anchor.y
        var width = abs(dx)
        var height = abs(dy)
        if let aspect = aspectRatio(portrait: height * size.height > width * size.width) {
            let normalized = aspect * size.height / size.width
            width = max(width, height * normalized)
            let maxWidth = dx >= 0 ? 1 - anchor.x : anchor.x
            let maxHeight = dy >= 0 ? 1 - anchor.y : anchor.y
            width = min(width, maxWidth, maxHeight * normalized)
            height = width / normalized
        }
        return CGRect(x: dx >= 0 ? anchor.x : anchor.x - width,
                      y: dy >= 0 ? anchor.y : anchor.y - height, width: width, height: height)
    }

    private func bounded(_ point: CGPoint, size: CGSize) -> CGPoint {
        CGPoint(x: min(1, max(0, point.x / size.width)), y: min(1, max(0, point.y / size.height)))
    }

    @MainActor private func loadImage() async {
        image = nil
        session = nil
        loadError = nil
        saveError = nil
        do {
            let prepared = try await geometryTasks.prepareCrop(assetID: asset.id)
            let proxy = try await SmartProxyCache.shared.proxy(source: prepared.sourceURL,
                identity: prepared.sourceID, configuration: prepared.configuration)
            let loaded = try await Task.detached {
                let image = try AIEditingImages.comparisonPreview(proxy)
                let oriented = try GeometryRenderer.geometry(CIImage(cgImage: image), quarterTurns: prepared.quarterTurns, crop: nil)
                guard let result = CIContext().createCGImage(oriented, from: oriented.extent) else {
                    throw GeometryJobError.missingRender
                }
                return result
            }.value
            try Task.checkCancellation()
            session = prepared
            image = loaded
            let rect = prepared.selection
            selection = CGRect(x: rect.minX, y: max(0, 1 - rect.maxY), width: rect.width, height: rect.height)
            ratio = prepared.ratio.flatMap(CropRatio.init(rawValue:)) ??
                (abs(rect.width - rect.height) < 0.000001 ? .original : .free)
        } catch {
            guard !Task.isCancelled else { return }
            loadError = error.localizedDescription + "\n" + String(reflecting: error)
        }
    }
}

private enum CropRatio: String, CaseIterable {
    case original = "原图比例", free = "自由", square = "1:1", fourThree = "4:3", threeTwo = "3:2", sixteenNine = "16:9"
}

private enum CropDrag {
    case resize(CGPoint)
    case move(CGPoint, CGRect)
}

private struct CropPointerInput: NSViewRepresentable {
    let enabled: Bool
    let changed: (CGPoint, CGPoint) -> Void
    let ended: () -> Void

    func makeNSView(context: Context) -> CropPointerView { CropPointerView() }
    func updateNSView(_ view: CropPointerView, context: Context) {
        view.enabled = enabled
        view.changed = changed
        view.ended = ended
    }
}

private final class CropPointerView: NSView {
    var enabled = true
    var changed: ((CGPoint, CGPoint) -> Void)?
    var ended: (() -> Void)?
    private var start: CGPoint?
    private var dragged = false
    override var isFlipped: Bool { true }
    override var mouseDownCanMoveWindow: Bool { false }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func mouseDown(with event: NSEvent) {
        guard enabled else { return }
        start = convert(event.locationInWindow, from: nil)
        dragged = false
    }
    override func mouseDragged(with event: NSEvent) {
        guard enabled, let start else { return }
        dragged = true
        changed?(start, convert(event.locationInWindow, from: nil))
    }
    override func mouseUp(with event: NSEvent) {
        if enabled, dragged, let start {
            changed?(start, convert(event.locationInWindow, from: nil))
        }
        start = nil
        dragged = false
        ended?()
    }
}
