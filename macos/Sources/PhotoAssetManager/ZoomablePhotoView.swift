import AppKit
import KeepsAPI
import SwiftUI

struct ZoomablePhotoView: View {
    let asset: KeepsAsset
    let configuration: KeepsConfiguration?
    @State private var image: CGImage?
    @State private var error: String?
    @State private var retry = 0
    @EnvironmentObject private var geometryTasks: GeometryTaskStore
    @State private var exposureSession: PhotoCropSession?
    @State private var exposureProxy: URL?
    @State private var exposureEV = 0.0
    @State private var editingExposure = false
    @State private var needsOriginal = false
    @State private var original: CGImage?

    var body: some View {
        ZStack {
            KeepsPreviewImage(asset: asset, configuration: configuration)
            if let image {
                PhotoCanvas(image: editingExposure ? image : (original ?? image),
                    logicalSize: asset.standard.map { CGSize(width: $0.width, height: $0.height) },
                    needsOriginal: { if !editingExposure { needsOriginal = true } })
            }
        }
        .overlay(alignment: .topTrailing) {
            if !asset.isVideo, asset.standard != nil, !editingExposure {
                Button { editingExposure = true } label: { Image(systemName: "plusminus.circle") }
                    .help("调整曝光").accessibilityLabel("调整曝光")
                    .disabled(asset.trashed || geometryTasks.isPending(assetID: asset.id))
                    .padding(8)
            }
        }
        .safeAreaInset(edge: .bottom) {
            if editingExposure {
                HStack(spacing: 12) {
                    Image(systemName: "plusminus.circle").accessibilityLabel("曝光")
                    Slider(value: $exposureEV, in: -2...2).frame(minWidth: 80, maxWidth: 240)
                        .disabled(exposureProxy == nil)
                    Text(String(format: "%+.2f EV", exposureEV)).monospacedDigit().frame(width: 80)
                    if exposureSession == nil, error == nil { ProgressView().controlSize(.small) }
                    Spacer(minLength: 8)
                    Button("取消") { editingExposure = false }
                    Button("应用") {
                        guard let exposureSession else { return }
                        do {
                            try geometryTasks.exposure(session: exposureSession, ev: exposureEV)
                            editingExposure = false
                        } catch { self.error = error.localizedDescription + "\n" + String(reflecting: error) }
                    }.disabled(exposureSession == nil || geometryTasks.isPending(assetID: asset.id))
                }.padding(12).background(WorkspaceStyle.panel)
            }
        }
        .overlay(alignment: .bottom) {
            if let error {
                VStack {
                    Button("重新载入清晰图像") { retry += 1 }
                    Text(error).font(.caption2).lineLimit(3).textSelection(.enabled)
                }.padding(8).background(.regularMaterial)
            }
        }
        .help("双击切换适应窗口与 1:1；滚轮缩放；按住鼠标左键拖动")
        .task(id: "\(retry)|\(editingExposure)") {
            guard let configuration, !asset.isVideo, !editingExposure else { return }
            error = nil
            do {
                let loaded = try await SmartProxyCache.shared.display(asset: asset, configuration: configuration)
                try Task.checkCancellation()
                image = loaded
            } catch {
                guard !Task.isCancelled else { return }
                self.error = String(reflecting: error)
            }
        }
        .task(id: needsOriginal) {
            guard needsOriginal, !editingExposure, let configuration else { return }
            do {
                let source = try await SmartProxyCache.shared.standard(asset: asset, configuration: configuration)
                let full = try await CullingFullSizeCache.shared.decode(url: source, maxPixelSize: nil)
                try Task.checkCancellation()
                original = full
            } catch {
                guard !Task.isCancelled else { return }
                self.error = String(reflecting: error)
            }
        }
        .task(id: "\(editingExposure)|\(retry)") {
            error = nil
            exposureEV = 0
            exposureSession = nil
            exposureProxy = nil
            guard let configuration else { return }
            do {
                if editingExposure {
                    let session = try await geometryTasks.prepareExposure(assetID: asset.id)
                    let proxy = try await SmartProxyCache.shared.proxy(source: session.sourceURL,
                        identity: session.sourceID, configuration: configuration)
                    try Task.checkCancellation()
                    exposureProxy = proxy
                    exposureSession = session
                }
            } catch {
                guard !Task.isCancelled else { return }
                self.error = error.localizedDescription + "\n" + String(reflecting: error)
            }
        }
        .task(id: "\(exposureProxy?.path ?? "")|\(exposureEV)") {
            guard editingExposure, let exposureProxy else { return }
            do {
                try await Task.sleep(for: .milliseconds(40))
                let preview = try await SmartProxyCache.shared.render(proxy: exposureProxy, exposureEV: exposureEV)
                try Task.checkCancellation()
                image = preview
            } catch {
                guard !Task.isCancelled else { return }
                self.error = String(reflecting: error)
            }
        }
    }
}

private struct PhotoCanvas: NSViewRepresentable {
    let image: CGImage
    let logicalSize: CGSize?
    let needsOriginal: () -> Void
    func makeNSView(context: Context) -> PhotoCanvasView {
        PhotoCanvasView(image: image, logicalSize: logicalSize)
    }
    func updateNSView(_ view: PhotoCanvasView, context: Context) {
        view.needsOriginal = needsOriginal
        view.update(image: image, preserveViewport: true)
    }
}

final class PhotoCanvasView: NSView {
    private var image: CGImage
    private let logicalSize: CGSize?
    var needsOriginal: (() -> Void)?
    private var imageSize: CGSize { logicalSize ?? CGSize(width: image.width, height: image.height) }
    private(set) var zoom: CGFloat = 1
    private(set) var offset = CGPoint.zero
    private var fitsWindow = true
    override var isFlipped: Bool { true }

    init(image: CGImage, logicalSize: CGSize? = nil) {
        self.image = image
        self.logicalSize = logicalSize
        super.init(frame: .zero)
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    var fitZoom: CGFloat {
        min(bounds.width / imageSize.width, bounds.height / imageSize.height)
    }
    var pixelZoom: CGFloat { 1 / (window?.backingScaleFactor ?? 1) }
    private var imageRect: CGRect {
        let size = CGSize(width: imageSize.width * zoom, height: imageSize.height * zoom)
        return CGRect(x: (bounds.width - size.width) / 2 + offset.x,
                      y: (bounds.height - size.height) / 2 + offset.y,
                      width: size.width, height: size.height)
    }

    func update(image: CGImage, preserveViewport: Bool = false) {
        guard self.image !== image else { return }
        self.image = image
        if !preserveViewport { fitsWindow = true }
        resizeImage()
    }
    override func layout() { super.layout(); resizeImage() }
    override func viewDidChangeBackingProperties() { super.viewDidChangeBackingProperties(); resizeImage() }

    private func resizeImage() {
        if fitsWindow { zoom = fitZoom; offset = .zero }
        constrainOffset()
        needsDisplay = true
    }
    private func constrainOffset() {
        let x = max(0, (imageSize.width * zoom - bounds.width) / 2)
        let y = max(0, (imageSize.height * zoom - bounds.height) / 2)
        offset.x = min(x, max(-x, offset.x))
        offset.y = min(y, max(-y, offset.y))
    }

    override func draw(_ dirtyRect: NSRect) {
        NSColor(calibratedWhite: 0.15, alpha: 1).setFill()
        bounds.fill()
        NSImage(cgImage: image, size: CGSize(width: image.width, height: image.height))
            .draw(in: imageRect, from: .zero, operation: .copy, fraction: 1,
                  respectFlipped: true, hints: [.interpolation: NSImageInterpolation.high])
    }

    func setZoom(_ requested: CGFloat, around point: CGPoint) {
        guard zoom > 0, fitZoom > 0 else { return }
        let next = min(max(fitZoom, pixelZoom * 8), max(min(fitZoom, pixelZoom), requested))
        let ratio = next / zoom
        let relative = CGPoint(x: point.x - bounds.midX, y: point.y - bounds.midY)
        offset = CGPoint(x: relative.x - (relative.x - offset.x) * ratio,
                         y: relative.y - (relative.y - offset.y) * ratio)
        zoom = next
        if next > fitZoom { needsOriginal?() }
        fitsWindow = abs(zoom - fitZoom) < 0.000001
        constrainOffset()
        needsDisplay = true
    }

    override func mouseDown(with event: NSEvent) {
        guard event.clickCount == 2 else { return }
        if fitsWindow {
            setZoom(pixelZoom, around: convert(event.locationInWindow, from: nil))
        } else {
            fitsWindow = true
            resizeImage()
        }
    }
    override func mouseDragged(with event: NSEvent) {
        offset.x += event.deltaX
        offset.y += event.deltaY
        constrainOffset()
        needsDisplay = true
    }
    override func scrollWheel(with event: NSEvent) {
        let sensitivity: CGFloat = event.hasPreciseScrollingDeltas ? 0.01 : 0.12
        setZoom(zoom * exp(-event.scrollingDeltaY * sensitivity),
                around: convert(event.locationInWindow, from: nil))
    }
    override func magnify(with event: NSEvent) {
        setZoom(zoom * (1 + event.magnification), around: convert(event.locationInWindow, from: nil))
    }
}
