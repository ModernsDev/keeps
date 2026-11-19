import SwiftUI

public struct KeepsAssetVersionsView: View {
    public let assetID: UUID
    public let revision: String
    public let configuration: KeepsConfiguration
    public var onSelection: (KeepsAsset) -> Void
    public var onSetNegative: ((KeepsAssetVersion) -> Void)?
    public var negativeChangePending: Bool
    @State private var versions: [KeepsAssetVersion] = []
    @State private var deprecatedFiles: [KeepsAssetVersions.DeprecatedFile] = []
    @State private var busy = false
    @State private var error: String?
    @State private var negativeContentHash: String?
    @State private var hasLoaded = false
    @State private var recycleTask: KeepsVersionRecycleTask?
    @State private var showRecyclePreview = false
    @State private var recycleCount = ""
    @State private var recycleError: String?

    public init(assetID: UUID, revision: String, configuration: KeepsConfiguration,
                onSetNegative: ((KeepsAssetVersion) -> Void)? = nil, negativeChangePending: Bool = false,
                onSelection: @escaping (KeepsAsset) -> Void) {
        self.assetID = assetID
        self.revision = revision
        self.configuration = configuration
        self.onSelection = onSelection
        self.onSetNegative = onSetNegative
        self.negativeChangePending = negativeChangePending
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("文件版本").font(.headline)
            if onSetNegative != nil, hasLoaded, !busy, error == nil, negativeContentHash == nil {
                Text("底片缺失，请右键点击文件版本并选择“设为底片”。").font(.caption).foregroundStyle(.secondary)
            }
            ForEach(versions) { version in
                VStack(alignment: .leading, spacing: 4) {
                    Text(version.filename).textSelection(.enabled)
                    Text("\(version.width) × \(version.height) · \(version.kind)").font(.caption).foregroundStyle(.secondary)
                    if version.contentHash == negativeContentHash {
                        Text("编辑底片").font(.caption).foregroundStyle(.secondary)
                    }
                    if version.isDefault {
                        Text(version.userSelected ? "默认展示 · 已手动指定" : "默认展示").font(.caption)
                    } else {
                        Button("设为默认展示") { Task { await select(version) } }
                            .disabled(busy || !version.available)
                    }
                    if !version.available { Text("文件暂不可用").font(.caption).foregroundStyle(.secondary) }
                    ForEach(version.paths.filter(\.available), id: \.path) { location in
                        HStack {
                            Text(location.path).font(.caption).textSelection(.enabled)
                            if versions.contains(where: { $0.paths.contains { $0.available && $0.path != location.path } }) {
                                Menu("回收此文件") {
                                    ForEach(availablePaths.filter { $0 != location.path }, id: \.self) { keep in
                                        Button("保留 \(keep)") { Task { await previewRecycle(keep: keep, recycle: location.path) } }
                                    }
                                }
                                .disabled(busy)
                            }
                        }
                    }
                }
                .contentShape(Rectangle())
                .contextMenu {
                    if let onSetNegative {
                        Button("设为底片") { onSetNegative(version) }
                            .disabled(busy || negativeChangePending || !version.available || version.contentHash == negativeContentHash)
                    }
                }
            }
            if !versions.isEmpty {
                Button("打开已有回收预览") { Task { await openExistingRecyclePreview() } }
                    .font(.caption)
                    .disabled(busy)
            }
            if !deprecatedFiles.isEmpty {
                DisclosureGroup("已弃用的重复文件") {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("以下文件仅标记为弃用，磁盘文件未删除。").foregroundStyle(.secondary)
                        ForEach(deprecatedFiles, id: \.path) { file in
                            VStack(alignment: .leading, spacing: 4) {
                                Text(file.path)
                                Text("保留位置：\(file.retainedPath)")
                                Text("弃用原因：\(file.reason)").foregroundStyle(.secondary)
                            }
                            .textSelection(.enabled)
                        }
                    }
                    .font(.caption)
                }
            }
            if versions.isEmpty && !busy && error == nil {
                Text("暂无版本信息，保留当前预览。").font(.caption).foregroundStyle(.secondary)
            }
            if busy { ProgressView() }
            if let error { Text(error).font(.caption).foregroundStyle(.red).textSelection(.enabled) }
        }
        .task(id: assetID.uuidString + ":" + revision) { await load() }
        .sheet(isPresented: $showRecyclePreview) { recyclePreview }
    }

    private var availablePaths: [String] {
        Array(Set(versions.flatMap { $0.paths.filter(\.available).map(\.path) })).sorted()
    }

    private var recyclePreview: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("版本回收预览").font(.headline)
            if let task = recycleTask {
                ScrollView {
                    VStack(alignment: .leading, spacing: 8) {
                        ForEach(task.versions, id: \.pick.recyclePath) { row in
                            Text("保留 \(row.pick.keepPath)")
                            Text("归位到 \(row.destination)")
                            if row.changesDefault {
                                Text("将默认展示切换到保留版本")
                            }
                            if row.changesEditingNegative {
                                Text("将编辑底片切换到保留版本")
                            }
                            ForEach(row.retainedFiles.dropFirst(), id: \.path) { file in
                                Text("保留 XMP \(file.path)")
                            }
                            ForEach(row.recycleFiles, id: \.self) { file in
                                Text("送入 NAS 回收站 \(file)")
                            }
                        }
                    }
                    .textSelection(.enabled)
                }
                if task.status == "draft" {
                    TextField("输入照片数量 \(task.count)", text: $recycleCount)
                    Button("确认回收", role: .destructive) {
                        Task { await confirmRecycle(task) }
                    }
                    .disabled(busy || recycleCount != String(task.count))
                } else if task.status == "pending" || task.status == "running" {
                    ProgressView(value: Double(task.completedFiles), total: Double(task.totalFiles))
                } else if task.status == "completed" {
                    Text("版本回收和归位已完成")
                }
                if let message = recycleError ?? task.error {
                    Text(message).foregroundStyle(.red).textSelection(.enabled)
                }
            }
            Button("关闭") { showRecyclePreview = false }
        }
        .padding()
        .frame(idealWidth: 560, minHeight: 330)
    }

    private func previewRecycle(keep: String, recycle: String) async {
        busy = true; error = nil; recycleError = nil; recycleCount = ""
        defer { busy = false }
        do {
            let pick = KeepsVersionRecyclePick(assetID: assetID, keepPath: keep, recyclePath: recycle)
            recycleTask = try await KeepsClient(configuration: configuration).previewVersionRecycle([pick])
            showRecyclePreview = true
        } catch { if !Task.isCancelled { self.error = String(reflecting: error) } }
    }

    private func openExistingRecyclePreview() async {
        busy = true; error = nil; recycleError = nil; recycleCount = ""
        defer { busy = false }
        do {
            let client = KeepsClient(configuration: configuration)
            let task = try await client.versionRecycleDraft(assetID: assetID)
            recycleTask = task
            showRecyclePreview = true
            if task.status != "draft" { try await monitorRecycle(task, client: client) }
        } catch { if !Task.isCancelled { self.error = String(reflecting: error) } }
    }

    private func monitorRecycle(_ initial: KeepsVersionRecycleTask, client: KeepsClient) async throws {
        var task = initial
        recycleTask = task
        while task.status == "pending" || task.status == "running" {
            try await Task.sleep(for: .seconds(1))
            task = try await client.versionRecycleTask(id: initial.id)
            recycleTask = task
        }
        if task.status == "completed" {
            let updated = try await client.asset(id: assetID)
            onSelection(updated)
            await load()
        }
    }

    private func confirmRecycle(_ preview: KeepsVersionRecycleTask) async {
        guard !busy, recycleCount == String(preview.count) else { return }
        busy = true; recycleError = nil
        defer { busy = false }
        do {
            let client = KeepsClient(configuration: configuration)
            let task = try await client.confirmVersionRecycle(id: preview.id, count: preview.count)
            try await monitorRecycle(task, client: client)
        } catch { if !Task.isCancelled { recycleError = String(reflecting: error) } }
    }

    private func load() async {
        versions = []; deprecatedFiles = []; error = nil; busy = true; hasLoaded = false
        defer { busy = false }
        do {
            let result = try await KeepsClient(configuration: configuration).versionDetails(assetID: assetID)
            try Task.checkCancellation()
            versions = result.items
            negativeContentHash = result.negativeContentHash ?? result.items.first(where: { $0.isNegative == true })?.contentHash
            deprecatedFiles = result.deprecatedFiles ?? []
            hasLoaded = true
        } catch { if !Task.isCancelled { self.error = String(reflecting: error) } }
    }

    private func select(_ version: KeepsAssetVersion) async {
        busy = true; error = nil
        defer { busy = false }
        do {
            let client = KeepsClient(configuration: configuration)
            versions = try await client.setDefaultVersion(assetID: assetID, contentHash: version.contentHash)
            let updated = try await client.asset(id: assetID)
            try Task.checkCancellation()
            onSelection(updated)
        } catch { if !Task.isCancelled { self.error = String(reflecting: error) } }
    }
}
