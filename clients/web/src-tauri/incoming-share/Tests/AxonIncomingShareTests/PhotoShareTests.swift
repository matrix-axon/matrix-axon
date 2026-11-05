import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers
import XCTest

@testable import AxonIncomingShare

enum PhotoFixture {
  static func write(_ url: URL, type: UTType = .png, width: Int = 32, height: Int = 24) throws {
    let context = try XCTUnwrap(
      CGContext(
        data: nil, width: width, height: height,
        bitsPerComponent: 8, bytesPerRow: width * 4, space: CGColorSpaceCreateDeviceRGB(),
        bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue))
    context.setFillColor(CGColor(red: 0.2, green: 0.4, blue: 0.6, alpha: 1))
    context.fill(CGRect(x: 0, y: 0, width: width, height: height))
    let image = try XCTUnwrap(context.makeImage())
    let destination = try XCTUnwrap(
      CGImageDestinationCreateWithURL(url as CFURL, type.identifier as CFString, 1, nil))
    CGImageDestinationAddImage(destination, image, nil)
    XCTAssertTrue(CGImageDestinationFinalize(destination))
  }
}

final class PhotoShareTests: XCTestCase {
  private var root: URL!
  private var store: ShareStore!
  override func setUpWithError() throws {
    root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    store = try ShareStore(root: root)
    try store.publish(
      ShareSession(
        installation: "test", server: "https://axon.example",
        credentialAvailable: true, expiresAt: nil), credential: {})
  }
  override func tearDownWithError() throws { try FileManager.default.removeItem(at: root) }

  func testOriginalExceedsPreviewCeilingAndUsesDetectedMime() throws {
    let source = root.appendingPathComponent("misleading.heic")
    try PhotoFixture.write(source)
    let handle = try FileHandle(forWritingTo: source)
    try handle.truncate(atOffset: UInt64(ShareLimits.thumbnailBytes + 1))
    try handle.close()
    var draft = try store.claim(store.create(text: "").id)
    draft = try store.importFile(
      draft.id, owner: draft.owner!, source: source,
      name: "original.heic", mime: "image/heic", photo: true)
    XCTAssertEqual(draft.files[0].mime, "image/png")
    XCTAssertEqual(draft.files[0].bytes, ShareLimits.thumbnailBytes + 1)
    XCTAssertFalse(draft.files[0].thumbnail)
    XCTAssertNil(TextShare.validation(draft))
    try store.withVerifiedFile(draft.id, file: draft.files[0].id, owner: draft.owner!) { url, _ in
      XCTAssertEqual(try Data(contentsOf: url), try Data(contentsOf: source))
    }
  }

  func testHEICOriginalAndBoundedDisplayDecode() throws {
    let source = root.appendingPathComponent("photo.heic")
    try PhotoFixture.write(source, type: .heic, width: 1800, height: 1200)
    XCTAssertTrue(try PhotoShare.mime(source).hasPrefix("image/hei"))
    let data = try PhotoShare.thumbnail(source)
    XCTAssertLessThan(data.count, ShareLimits.thumbnailBytes)
    let preview = try XCTUnwrap(CGImageSourceCreateWithData(data as CFData, nil))
    let properties = try XCTUnwrap(
      CGImageSourceCopyPropertiesAtIndex(preview, 0, nil) as? [CFString: Any])
    XCTAssertLessThanOrEqual(try XCTUnwrap(properties[kCGImagePropertyPixelWidth] as? Int), 480)
    XCTAssertLessThanOrEqual(try XCTUnwrap(properties[kCGImagePropertyPixelHeight] as? Int), 480)
  }

  func testInvalidAndOverLimitPhotosNeverPublishAFile() throws {
    let source = root.appendingPathComponent("not-a-photo.png")
    try Data("synthetic invalid image".utf8).write(to: source)
    let draft = try store.claim(store.create(text: "").id)
    XCTAssertThrowsError(
      try store.importFile(
        draft.id, owner: draft.owner!, source: source,
        name: "photo.png", mime: "image/png", photo: true))
    let handle = try FileHandle(forWritingTo: source)
    try handle.truncate(atOffset: UInt64(ShareLimits.batchBytes + 1))
    try handle.close()
    XCTAssertThrowsError(
      try store.importFile(
        draft.id, owner: draft.owner!, source: source,
        name: "photo.png", mime: "image/png", photo: true)
    ) { XCTAssertEqual($0 as? ShareFailure, .quota) }
    XCTAssertTrue(try store.list()[0].files.isEmpty)
  }

  func testCancellationDuringCopyDoesNotPublishAndReleasesReservations() throws {
    let source = root.appendingPathComponent("photo.png")
    try PhotoFixture.write(source)
    let handle = try FileHandle(forWritingTo: source)
    try handle.truncate(atOffset: 256 * 1024)
    try handle.close()
    let draft = try store.claim(store.create(text: "").id)
    var checks = 0
    XCTAssertThrowsError(
      try store.importFile(
        draft.id, owner: draft.owner!, source: source,
        name: "photo.png", mime: "image/png", photo: true,
        check: {
          checks += 1
          if checks > 2 { throw ShareFailure.cancelled }
        })
    ) { XCTAssertEqual($0 as? ShareFailure, .cancelled) }
    XCTAssertTrue(try store.list()[0].files.isEmpty)
    let files = try FileManager.default.contentsOfDirectory(
      atPath: root.appendingPathComponent(draft.id).path)
    XCTAssertFalse(files.contains { $0.contains(".part") })
  }

  func testRemovalFencesOldRevisionsAndCannotShiftADeliveredCaption() throws {
    let source = root.appendingPathComponent("photo.png")
    try PhotoFixture.write(source)
    var draft = try store.claim(store.create(text: "Caption").id)
    for name in ["one.png", "two.png"] {
      draft = try store.importFile(
        draft.id, owner: draft.owner!, source: source,
        name: name, mime: "image/png", photo: true)
    }
    let before = draft
    draft = try store.removeFile(draft, file: draft.files[0].id)
    XCTAssertEqual(draft.files.map(\.name), ["two.png"])
    XCTAssertThrowsError(try store.update(before))
    draft.files[0].operation!.state = .dispatching
    draft = try store.update(draft)
    XCTAssertThrowsError(try store.removeFile(draft, file: draft.files[0].id))
    draft.files[0].operation!.state = .sent
    draft.files[0].operation!.eventId = "$confirmed"
    draft = try store.update(draft)
    XCTAssertThrowsError(try store.removeFile(draft, file: draft.files[0].id))
  }
}
