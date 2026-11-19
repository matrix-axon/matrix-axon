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
    struct HeadReadLimits: Decodable {
      let initial: Int
      let maximum: Int
      let tags: Int
    }
    struct HeadRead: Decodable {
      let name: String
      let contentType: String?
      let start: String
      let padTo: Int
      let tail: String
      let `repeat`: String?
      let repetitions: Int?
      let repeatAfterPadding: Bool?
      let trailingSpaces: Int?
      let stop: String
      let title: String?
      let description: String?
      let image: String?
    }
    let headReadLimits: HeadReadLimits
    let headReads: [HeadRead]
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
      let bytes = Data(entry.text.utf8)
      XCTAssertEqual(PreviewHTTPParser.endsHead(bytes), entry.ends, entry.text)
      for split in bytes.indices.dropFirst() where !PreviewHTTPParser.endsHead(bytes.prefix(split)) {
        XCTAssertEqual(PreviewHTTPParser.endsHead(bytes, previous: split), entry.ends, entry.text)
      }
    }
    // A marker split across reads ends the head once its boundary arrives.
    var parser = PreviewHTTPParser(kind: .page)
    XCTAssertFalse(
      try parser.append(Data("HTTP/1.1 200 OK\r\n\r\n<title>T</title><bodyguard><body".utf8)))
    XCTAssertTrue(try parser.append(Data(">".utf8)))
  }

  func testHeadReads() throws {
    let url = try XCTUnwrap(URL(string: "https://page.example.org/"))
    let fixture = try fixture()
    XCTAssertEqual(RestrictedPreview.initialHeadBytes, fixture.headReadLimits.initial)
    XCTAssertEqual(RestrictedPreview.maxHeadBytes, fixture.headReadLimits.maximum)
    XCTAssertEqual(RestrictedPreview.maxTags, fixture.headReadLimits.tags)
    for entry in fixture.headReads {
      let repeated = String(repeating: entry.repeat ?? "", count: entry.repetitions ?? 0)
      var body = Data(entry.start.utf8)
      if entry.repeatAfterPadding != true { body.append(contentsOf: repeated.utf8) }
      body.append(Data(repeating: 32, count: entry.padTo - body.count))
      body.append(contentsOf: entry.tail.utf8)
      if entry.repeatAfterPadding == true { body.append(contentsOf: repeated.utf8) }
      body.append(Data(repeating: 32, count: entry.trailingSpaces ?? 0))
      let type = entry.contentType ?? "text/html"
      let head = "HTTP/1.1 200 OK\r\nContent-Type: \(type)\r\nContent-Length: \(body.count)\r\n\r\n"
      var wire = Data(head.utf8)
      wire.append(body)
      let now = DispatchTime.now()
      var read = PreviewNetwork.ReadState(kind: .page)
      var done = false
      for start in stride(from: 0, to: wire.count, by: 16 * 1024) {
        let end = min(start + 16 * 1024, wire.count)
        done = try read.append(
          wire[start..<end], eof: end == wire.count, now: now, deadline: now + 25)
        if done { break }
      }
      XCTAssertTrue(done, entry.name)
      let response = read.response(url: url)
      let retained = response.body.count
      switch entry.stop {
      case "full": XCTAssertEqual(retained, body.count, entry.name)
      case "initial": XCTAssertEqual(retained, RestrictedPreview.initialHeadBytes, entry.name)
      case "hard": XCTAssertEqual(retained, RestrictedPreview.maxHeadBytes, entry.name)
      case "headers": XCTAssertEqual(retained, 0, entry.name)
      case "tagBudget":
        XCTAssertTrue((entry.padTo..<(entry.padTo + 32 * 1024)).contains(retained), entry.name)
      default: XCTFail(entry.name)
      }
      if case .html(let contentType) = response.media {
        let page = try RestrictedPreview.parse(
          response.body, contentType: contentType, cutoff: response.cutoff)
        XCTAssertEqual(page.title, entry.title, entry.name)
        XCTAssertEqual(page.description, entry.description, entry.name)
        XCTAssertEqual(page.image, entry.image, entry.name)
      } else {
        XCTAssertEqual(response.media, .other, entry.name)
        XCTAssertEqual(entry.stop, "headers", entry.name)
      }
    }
  }

  func testOptionalReadReturnsPrefixBeforeSharedDeadline() throws {
    let now = DispatchTime.now()
    let deadline = now + 25
    let url = try XCTUnwrap(URL(string: "https://page.example.org/"))
    let headers = Data("HTTP/1.1 200 OK\r\nContent-Length: 1100000\r\n\r\n".utf8)
    let block = Data(repeating: 32, count: 16 * 1024)
    for (checkpoint, softDeadline) in [(9, 13), (18, 19), (21, nil)] as [(Int, Int?)] {
      var read = PreviewNetwork.ReadState(kind: .page)
      let atCheckpoint = now + .seconds(checkpoint)
      XCTAssertFalse(try read.append(headers, eof: false, now: now, deadline: deadline))
      for _ in 0..<15 {
        XCTAssertFalse(try read.append(block, eof: false, now: atCheckpoint, deadline: deadline))
      }
      let done = try read.append(block, eof: false, now: atCheckpoint, deadline: deadline)
      XCTAssertEqual(done, softDeadline == nil)
      XCTAssertEqual(read.extensionDeadline, softDeadline.map { now + .seconds($0) })
      if let softDeadline {
        XCTAssertTrue(try read.append(Data(), eof: false, now: now + .seconds(softDeadline), deadline: deadline))
      }
      let response = read.response(url: url)
      XCTAssertTrue(response.cutoff)
      XCTAssertEqual(response.body.count, RestrictedPreview.initialHeadBytes)
      XCTAssertNil(try RestrictedPreview.parse(response.body, cutoff: true).title)
    }
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
