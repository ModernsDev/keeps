import Foundation
import KeepsAPI

struct PhotoEditXMP: Codable, Equatable, Sendable {
    static let namespace = "https://keeps.photos/ns/edit/1.0/"
    enum Operation: String, Codable { case geometry, exposure, displayExposure, color }
    var schema = 1
    var assetID: UUID
    var rootID: String
    var versionID: UUID
    var negativeHash: String
    var sourceVersion: String
    var operation: Operation
    var quarterTurns: Int?
    var crop: CGRect?
    var ratio: String?
    var exposureEV: Double?
    var recipe: KeepsEditRecipe?

    func xml() throws -> String {
        let description = XMLElement(name: "rdf:Description")
        description.addNamespace(XMLNode.namespace(withName: "keeps", stringValue: Self.namespace) as! XMLNode)
        description.addNamespace(XMLNode.namespace(withName: "xmpMM", stringValue: "http://ns.adobe.com/xap/1.0/mm/") as! XMLNode)
        description.addAttribute(XMLNode.attribute(withName: "rdf:about", stringValue: "") as! XMLNode)
        for (name, value) in [
            ("xmpMM:OriginalDocumentID", "xmp.did:" + rootID.lowercased()),
            ("xmpMM:DocumentID", "xmp.did:" + versionID.uuidString.lowercased())
        ] {
            description.addAttribute(XMLNode.attribute(withName: name, stringValue: value) as! XMLNode)
        }
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        description.addChild(XMLElement(name: "keeps:Edit", stringValue: String(decoding: try encoder.encode(self), as: UTF8.self)))
        let rdf = XMLElement(name: "rdf:RDF")
        rdf.addNamespace(XMLNode.namespace(withName: "rdf", stringValue: "http://www.w3.org/1999/02/22-rdf-syntax-ns#") as! XMLNode)
        rdf.addChild(description)
        let root = XMLElement(name: "x:xmpmeta")
        root.addNamespace(XMLNode.namespace(withName: "x", stringValue: "adobe:ns:meta/") as! XMLNode)
        root.addChild(rdf)
        return XMLDocument(rootElement: root).xmlString()
    }

    init(xml: String) throws {
        let document = try XMLDocument(xmlString: xml, options: [.nodeLoadExternalEntitiesNever])
        let nodes = try document.nodes(forXPath: "//*[local-name()='Edit']").filter { $0.uri == Self.namespace }
        guard nodes.count == 1, let text = nodes.first?.stringValue else { throw GeometryJobError.missingRender }
        self = try JSONDecoder().decode(Self.self, from: Data(text.utf8))
        guard schema == 1 else { throw GeometryJobError.sourceChanged }
    }

    init(assetID: UUID, rootID: String, versionID: UUID, negativeHash: String,
         sourceVersion: String, operation: Operation, quarterTurns: Int? = nil, crop: CGRect? = nil,
         ratio: String? = nil, exposureEV: Double? = nil, recipe: KeepsEditRecipe? = nil) {
        self.assetID = assetID
        self.rootID = rootID
        self.versionID = versionID
        self.negativeHash = negativeHash
        self.sourceVersion = sourceVersion
        self.operation = operation
        self.quarterTurns = quarterTurns
        self.crop = crop
        self.ratio = ratio
        self.exposureEV = exposureEV
        self.recipe = recipe
    }

    func save(to directory: URL) throws -> URL {
        let url = directory.appendingPathComponent("edit.xmp")
        try xml().write(to: url, atomically: true, encoding: .utf8)
        return url
    }
}
