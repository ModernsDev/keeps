import Foundation
import AppKit
import SwiftUI
import os
import Testing
import KeepsAPI
@testable import PhotoAssetManager

@MainActor struct PhotoSelectionTests {
    @Test func cullingUsesSelectedPhotoDirectoryOutsideDirectoryView() async throws {
        let store = try await loadedStore()
        #expect(store.cullingDirectory == nil)
        store.select(store.assets[0].id)
        #expect(store.query.directory == nil)
        #expect(store.cullingDirectory == "/photos/selected")
        store.query.directory = "/photos"
        #expect(store.cullingDirectory == "/photos")
        store.query.trashed = true
        #expect(store.cullingDirectory == nil)
    }

    @Test func rangeSelectionKeepsAnchorAndKeyboardFocus() async throws {
        let store = try await loadedStore()
        let ids = store.assets.map(\.id)
        store.select(ids[1])
        store.select(ids[4], range: true)
        #expect(store.selectedIDs == Set(ids[1...4]))
        store.selectAdjacent(-1, extending: true)
        #expect(store.selectedIDs == Set(ids[1...3]))
        store.selectAdjacent(-1, extending: true)
        #expect(store.selectedIDs == Set(ids[1...2]))
        store.select(ids[0], extending: true)
        #expect(store.selectedIDs == Set(ids[0...2]))
        store.select(ids[0], extending: true)
        #expect(store.selectedIDs == Set(ids[1...2]))
        store.select(ids[4], extending: true, range: true)
        #expect(store.selectedIDs == Set(ids))
        store.deselectAll()
        store.selectAdjacent(1)
        #expect(store.selectedIDs == [ids[0]])
    }

    @Test func selectAllIncludesUnloadedPages() async throws {
        let store = try await loadedStore(paginated: true)
        #expect(store.assets.count == 2)
        store.selectAll()
        try await waitUntil { !store.isLoading }
        #expect(store.lastError == nil)
        #expect(store.selectedIDs.count == 5)
        #expect(!store.isSelectingAll)
    }

    @Test(arguments: [false, true]) func restoringTargetKeepsOtherPhotosSelectedAndInTrash(fromDrop: Bool) async throws {
        let store = try await loadedStore(trashed: true)
        let selected = store.assets[0].id
        let target = store.assets[1].id
        store.select(selected)

        if fromDrop {
            let payload = try #require(store.photoDragPayload(for: target))
            #expect(store.canMovePhotos(payload, to: "/other"))
            let info = TestDragInfo(source: PhotoDragSource.SourceView())
            let type = NSPasteboard.PasteboardType(PhotoDragPayload.pasteboardType)
            info.draggingPasteboard.declareTypes([type], owner: nil)
            info.draggingPasteboard.setData(try JSONEncoder().encode(payload), forType: type)
            let dropTarget = RestorePhotosDropTarget<Text>.TargetView(rootView: Text("全部照片"))
            dropTarget.library = store
            #expect(dropTarget.draggingEntered(info) == .move)
            #expect(dropTarget.prepareForDragOperation(info))
            #expect(dropTarget.performDragOperation(info))
        } else {
            store.restorePhotos([target])
        }
        try await waitUntil { !store.isMutating && !store.isLoading && store.counts?.trashed == 4 }

        #expect(store.lastError == nil)
        #expect(store.assets.count == 4)
        #expect(!store.assets.contains { $0.id == target })
        #expect(store.selectedIDs == [selected])
        #expect(store.selectedAsset?.trashed == true)
    }

    @Test func deselectDuringPaginationDoesNotReselectLaterPage() async throws {
        let store = try await loadedStore(paginated: true)
        store.selectAll()
        store.deselectAll()
        try await waitUntil { !store.isLoading }
        #expect(store.assets.count == 5)
        #expect(store.selectedIDs.isEmpty)
    }

    @Test func changingDirectoryCancelsPendingSelectAll() async throws {
        let store = try await loadedStore(paginated: true)
        store.selectAll()
        store.showLibrary(directory: "/other")
        try await waitUntil { !store.isLoading }
        #expect(store.query.directory == "/other")
        #expect(store.selectedIDs.isEmpty)
        #expect(!store.isSelectingAll)
    }

    @Test func changingConnectionClearsSelectionAndPendingSelectAll() async throws {
        let store = try await loadedStore(paginated: true)
        store.selectAll()
        let connected = await store.checkConnection(baseURL: "https://all.invalid", libraryID: "new", accessCredential: "", save: true)
        #expect(connected)
        try await waitUntil { !store.isLoading }
        #expect(store.configuration?.libraryID == "new")
        #expect(store.selectedIDs.isEmpty)
        #expect(!store.isSelectingAll)
    }

    @Test(arguments: [false, true]) func aiActivityBlocksSelectionNavigationAndMutations(settingsTest: Bool) async throws {
        let store = try await loadedStore()
        let originalID = store.assets[0].id
        store.select(originalID)
        store.isAIEditingBlocking = !settingsTest
        store.isAISettingsBusy = settingsTest
        store.pauseLibraryForDirectoryOperation()

        store.select(store.assets[1].id)
        store.selectAdjacent(1)
        store.selectAll()
        store.deselectAll()
        store.showLibrary(directory: "/other")
        store.updateSelected(KeepsAssetPatch(rating: 5))
        store.refresh(force: true)
        let connected = await store.checkConnection(baseURL: "https://all.invalid", libraryID: "new", accessCredential: "", save: true)

        #expect(store.selectedIDs == [originalID])
        #expect(store.query.directory == nil)
        #expect(!store.isMutating)
        #expect(!store.isLoading)
        #expect(!connected)
        #expect(store.configuration?.libraryID == "selection")
        store.isAIEditingBlocking = false
        store.isAISettingsBusy = false
        store.select(store.assets[1].id)
        #expect(store.selectedIDs == [store.assets[1].id])
    }

    @Test(arguments: [false, true]) func batchTagProgressBlocksUntilCompletionOrFailure(fails: Bool) async throws {
        let store = try await loadedStore()
        let assets = Dictionary(uniqueKeysWithValues: store.assets.prefix(2).map { ($0.id, $0) })
        let ids = Set(assets.keys)
        store.selectedIDs = ids

        store.tagSelected(title: "正在添加标签…") { _, id in
            let completed = try #require(await store.batchTagProgress?.completed)
            #expect(await store.isOperationBlocking)
            if completed == 1 && fails { throw URLError(.networkConnectionLost) }
            return try #require(assets[id])
        }

        #expect(store.batchTagProgress?.total == 2)
        #expect(store.batchTagProgress?.completed == 0)
        store.deselectAll()
        #expect(store.selectedIDs == ids)

        try await waitUntil { !store.isMutating && !store.isLoading }
        #expect(store.batchTagProgress == nil)
        #expect(!store.isOperationBlocking)
        #expect(fails ? store.lastError?.contains("已完成 1/2 项") == true : store.lastError == nil)
    }

    @Test func finishingSettingsWorkDoesNotUnlockPhotoBatch() {
        let store = LibraryStore(loadSavedSettings: false, preferences: UserDefaults(suiteName: UUID().uuidString)!)
        store.isAIEditingBlocking = true
        store.isAISettingsBusy = true
        store.isAISettingsBusy = false
        #expect(store.isOperationBlocking)
        #expect(!store.isDirectoryOperationBlocking)
    }

    private func loadedStore(paginated: Bool = false, trashed: Bool = false) async throws -> LibraryStore {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [SelectionProtocol.self]
        let host = trashed ? "restore-\(UUID().uuidString.lowercased()).invalid" : paginated ? "pages.invalid" : "all.invalid"
        let store = LibraryStore(configuration: KeepsConfiguration(baseURL: URL(string: "https://\(host)")!, libraryID: "selection"), session: URLSession(configuration: configuration), loadSavedSettings: false, preferences: UserDefaults(suiteName: UUID().uuidString)!, persistConfiguration: { _ in })
        store.showLibrary(trashed: trashed)
        try await waitUntil { !store.isLoading }
        try #require(store.lastError == nil)
        return store
    }

    private func waitUntil(_ predicate: () -> Bool) async throws {
        for _ in 0..<200 {
            if predicate() { return }
            try await Task.sleep(for: .milliseconds(5))
        }
        Issue.record("selection request did not complete")
    }
}

private final class SelectionProtocol: URLProtocol, @unchecked Sendable {
    private static let restored = OSAllocatedUnfairLock(initialState: [String: Set<String>]())

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        let url = request.url!
        let host = url.host!
        let trash = host.hasPrefix("restore-")
        let restored = Self.restored.withLock { $0[host] ?? [] }
        let body: String
        func photo(_ suffix: String, trashed: Bool) -> String {
            """
            {"id":"00000000-0000-0000-0000-00000000000\(suffix)","cameraMake":"","cameraModel":"","lensModel":"","originalFilename":"photo.jpg","contentFingerprint":"hash","metadataFingerprint":"meta","rating":0,"flagState":"unflagged","tags":[],"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","trashed":\(trashed),"paths":["/photos/selected/photo.jpg"]}
            """
        }
        if url.path.hasSuffix("/assets") {
            let paginated = url.host == "pages.invalid"
            let cursor = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems?.contains { $0.name == "cursor" } == true
            let numbers = paginated ? (cursor ? Array(3...5) : Array(1...2)) : Array(1...5)
            let items = numbers.map { String($0) }.filter { !trash || !restored.contains($0) }
                .map { photo($0, trashed: trash) }.joined(separator: ",")
            let total = trash ? 5 - restored.count : 5
            body = "{\"items\":[\(items)],\"total\":\(total),\"revision\":1,\"isUpdating\":false" + (paginated && !cursor ? ",\"nextCursor\":\"next\"}" : "}")
        } else if trash, request.httpMethod == "POST", url.lastPathComponent == "restore",
                  let suffix = url.deletingLastPathComponent().lastPathComponent.last {
            Self.restored.withLock { _ = $0[host, default: []].insert(String(suffix)) }
            body = photo(String(suffix), trashed: false)
        } else if url.path.hasSuffix("/revision") { body = "{\"revision\":1,\"isUpdating\":false}" }
        else if url.path.hasSuffix("/navigation") { body = "{\"directories\":[]}" }
        else if url.path.hasSuffix("/hidden-directories") { body = "{\"paths\":[]}" }
        else if trash { body = "{\"all\":5,\"picked\":0,\"trashed\":\(5 - restored.count)}" }
        else { body = "{\"all\":5,\"picked\":0,\"trashed\":0}" }
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: url, statusCode: 200, httpVersion: nil, headerFields: ["Content-Type": "application/json"])!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(body.utf8))
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}
