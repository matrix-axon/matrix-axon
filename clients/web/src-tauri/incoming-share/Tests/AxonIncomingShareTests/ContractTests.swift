import Foundation
import XCTest

@testable import AxonIncomingShare

final class ContractTests: XCTestCase {
  func testEquivalentURLsAreDuplicates() {
    for other in ["https://example.org/", "https://EXAMPLE.org:443/"] {
      XCTAssertEqual(
        PreviewContract.check([
          "body": "https://example.org " + other,
          "url_previews": [["matched_url": "https://example.org"], ["matched_url": other]],
        ]), "invalid")
    }
  }
  func testPortableLocalManifestRoundTrip() throws {
    let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
      .appendingPathComponent("../../../../src/incoming-share/fixtures/manifest-v1.json")
      .standardizedFileURL
    let data = try Data(contentsOf: url)
    let manifest = try JSONDecoder().decode(ShareManifest.self, from: data)
    XCTAssertEqual(
      manifest.files.filter { !$0.thumbnail }.map(\.name), ["report.pdf", "second.txt"])
    XCTAssertEqual(manifest.files[1].operation?.eventId, "$confirmed")
    XCTAssertEqual(manifest.files[2].operation?.state, .unknown)
    XCTAssertFalse(manifest.completed)
    let original = try JSONSerialization.jsonObject(with: data) as! NSDictionary
    let encoded =
      try JSONSerialization.jsonObject(with: ShareJSON.encode(manifest)) as! NSDictionary
    XCTAssertEqual(original, encoded)
  }
  private var fixtures: URL {
    URL(fileURLWithPath: #filePath).deletingLastPathComponent()
      .appendingPathComponent("../../../../../../crates/axon-api/tests/fixtures")
      .standardizedFileURL
  }
  func testServerSendFixtures() throws {
    let fixture =
      try JSONSerialization.jsonObject(
        with: Data(contentsOf: fixtures.appendingPathComponent("link-preview-contract.json")))
      as! [String: Any]
    for entry in fixture["cases"] as! [[String: Any]] {
      let error = PreviewContract.check(entry["request"] as! [String: Any])
      let status = error == nil ? 200 : error == "invalid" ? 400 : 413
      XCTAssertEqual(status, entry["status"] as! Int, entry["name"] as! String)
      if let code = entry["code"] as? String { XCTAssertEqual(error, code) }
    }
  }
  func testServerGenerationFixtures() throws {
    let fixture =
      try JSONSerialization.jsonObject(
        with: Data(contentsOf: fixtures.appendingPathComponent("preview-generation-contract.json")))
      as! [String: Any]
    XCTAssertTrue(
      PreviewContract.validateWire(fixture["request"]!, schema: "GeneratePreviewRequest"))
    for outcome in fixture["outcomes"] as! [[String: Any]] {
      XCTAssertTrue(PreviewContract.validateWire(outcome["data"]!, schema: "GeneratedPreviewDto"))
    }
  }
}
