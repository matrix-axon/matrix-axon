import Foundation
import XCTest

@testable import AxonIncomingShare

/// The fetch boundary both device-preview readers enforce (ADR 0132 § 3).
/// The shell's Rust plugin reads the same file in `axon-link-preview/src/tests.rs`,
/// so a change to either reader that disagrees with a case here fails a test.
final class BoundaryFixtureTests: XCTestCase {
  private struct Fixture: Decodable {
    struct Addresses: Decodable {
      let `public`: [String]
      let nonPublic: [String]
    }
    struct NAT64: Decodable {
      struct Vector: Decodable {
        let length: Int
        let probe: String
        let translated: String
      }
      let translationTarget: String
      let vectors: [Vector]
      let malformedProbes: [String]
      let untranslatable: [String]
    }
    struct Resolution: Decodable {
      let name: String
      let a: String?
      let probe: String?
      let addresses: [String]?
      let failure: String?
    }
    struct Metadata: Decodable {
      let name: String
      let html: String
      let latin1: Bool?
      let contentType: String?
      let title: String?
      let description: String?
      let siteName: String?
      let image: String?
      let failure: String?
    }
    struct HeadEnd: Decodable {
      let text: String
      let ends: Bool
    }
    let rejectedBeforeDNS: [String]
    let headEnds: [HeadEnd]
    let addresses: Addresses
    let operatorMappings: [String]
    let nat64: NAT64
    let resolution: [Resolution]
    let metadata: [Metadata]
  }

  private func fixture() throws -> Fixture {
    let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
      .appendingPathComponent("../../../../src/incoming-share/fixtures/preview-boundary.json")
      .standardizedFileURL
    return try JSONDecoder().decode(Fixture.self, from: Data(contentsOf: url))
  }

  private func address(_ text: String) throws -> PreviewNetwork.Address {
    try XCTUnwrap(PreviewNetwork.Address.parse(text), text)
  }

  func testURLsRejectedBeforeDNS() throws {
    for raw in try fixture().rejectedBeforeDNS {
      // Foundation cannot represent some of these at all, which is a rejection too.
      guard let url = URL(string: raw) else { continue }
      XCTAssertThrowsError(
        try PreviewNetwork.fetch(
          url, kind: .page, deadline: .now() + 1, cancellation: ShareCancellation(),
          resolve: { _, _, _ in
            XCTFail("\(raw) reached DNS")
            throw ShareFailure.invalid
          }), raw)
    }
  }

  func testAddressScope() throws {
    let addresses = try fixture().addresses
    for text in addresses.public {
      XCTAssertTrue(try address(text).isPublic, text)
    }
    for text in addresses.nonPublic {
      XCTAssertFalse(try address(text).isPublic, text)
      XCTAssertFalse(try address(text).approvedForConnection, text)
    }
  }

  func testOperatorMappingsAreNeverApproved() throws {
    for text in try fixture().operatorMappings {
      let mapped = try address(text)
      XCTAssertTrue(mapped.isPublic, "Global IPv6 scope alone is not authorization: \(text)")
      XCTAssertFalse(mapped.approvedForConnection, text)
      for kind: PreviewNetwork.Kind in [.page, .image] {
        var connected = false
        XCTAssertThrowsError(
          try PreviewNetwork.fetch(
            URL(string: "https://example.org/")!, kind: kind, deadline: .now() + 1,
            cancellation: ShareCancellation(), resolve: { _, _, _ in [mapped] },
            exchange: { url, _, _, _, _, _ in
              connected = true
              return .init(status: 200, headers: [:], body: Data(), url: url)
            }))
        XCTAssertFalse(connected, text)
      }
    }
  }

  func testNAT64Vectors() throws {
    let nat64 = try fixture().nat64
    let target = try address(nat64.translationTarget)
    for vector in nat64.vectors {
      let prefix = try XCTUnwrap(
        PreviewNetwork.NAT64Prefix.discover(try address(vector.probe)), vector.probe)
      XCTAssertEqual(prefix.length, vector.length, vector.probe)
      let translated = try XCTUnwrap(PreviewNetwork.Address.translated(target, using: prefix))
      XCTAssertEqual(translated.bytes, try address(vector.translated).bytes, vector.probe)
      XCTAssertTrue(translated.approvedForConnection, vector.probe)
      // DNS-supplied copies of the same bits do not carry public-A provenance.
      XCTAssertFalse(try address(vector.translated).approvedForConnection, vector.probe)
      for unsafe in nat64.untranslatable {
        XCTAssertNil(PreviewNetwork.Address.translated(try address(unsafe), using: prefix), unsafe)
      }
    }
    for probe in nat64.malformedProbes {
      XCTAssertNil(PreviewNetwork.NAT64Prefix.discover(try address(probe)), probe)
    }
  }

  func testResolution() throws {
    func answer(_ text: String?) -> ShareIntakeResult<PreviewNetwork.Address?> {
      let result = ShareIntakeResult<PreviewNetwork.Address?>()
      result.finish(.success(text.flatMap(PreviewNetwork.Address.parse)))
      return result
    }
    for entry in try fixture().resolution {
      do {
        let addresses = try PreviewNetwork.resolvedAddresses(
          [answer(entry.a), answer(entry.probe)], deadline: .now() + 1,
          cancellation: ShareCancellation())
        XCTAssertNil(entry.failure, entry.name)
        XCTAssertEqual(addresses.map(\.bytes), try entry.addresses?.map { try address($0).bytes }, entry.name)
        XCTAssertTrue(addresses.allSatisfy(\.approvedForConnection), entry.name)
      } catch let failure as ShareFailure {
        XCTAssertEqual(failure.rawValue, entry.failure, entry.name)
      }
    }
  }

  func testHeadEnds() throws {
    for entry in try fixture().headEnds {
      XCTAssertEqual(PreviewHTTPParser.endsHead(Data(entry.text.utf8)), entry.ends, entry.text)
    }
    // A marker split across reads ends the head once its boundary arrives.
    var parser = PreviewHTTPParser(kind: .page)
    XCTAssertFalse(
      try parser.append(Data("HTTP/1.1 200 OK\r\n\r\n<title>T</title><bodyguard><body".utf8)))
    XCTAssertTrue(try parser.append(Data(">".utf8)))
  }

  func testMetadata() throws {
    for entry in try fixture().metadata {
      let data =
        entry.latin1 == true
        ? Data(entry.html.unicodeScalars.map { UInt8($0.value) }) : Data(entry.html.utf8)
      do {
        let page = try RestrictedPreview.parse(data, contentType: entry.contentType ?? "text/html")
        XCTAssertNil(entry.failure, entry.name)
        XCTAssertEqual(page.title, entry.title, entry.name)
        XCTAssertEqual(page.description, entry.description, entry.name)
        XCTAssertEqual(page.siteName, entry.siteName, entry.name)
        XCTAssertEqual(page.image, entry.image, entry.name)
      } catch let failure as ShareFailure {
        XCTAssertEqual(failure.rawValue, entry.failure, entry.name)
      }
    }
  }
}
