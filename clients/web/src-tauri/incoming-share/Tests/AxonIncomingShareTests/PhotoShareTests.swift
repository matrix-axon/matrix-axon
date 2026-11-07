import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers
import XCTest

@testable import AxonIncomingShare

enum PhotoFixture {
  static func write(
    _ url: URL, type: UTType = .png, width: Int = 32, height: Int = 24, alpha: Bool = false
  ) throws {
    let context = try XCTUnwrap(
      CGContext(
        data: nil, width: width, height: height,
        bitsPerComponent: 8, bytesPerRow: width * 4, space: CGColorSpaceCreateDeviceRGB(),
        bitmapInfo: (alpha ? CGImageAlphaInfo.premultipliedLast : .noneSkipLast).rawValue))
    context.setFillColor(CGColor(red: 0.2, green: 0.4, blue: 0.6, alpha: alpha ? 0.5 : 1))
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
    draft = try store.importThumbnail(
      draft.id, owner: draft.owner!, data: Data(contentsOf: source), mime: "image/png",
      preview: SharePreview(matched_url: "https://example.org", title: "First photo card"))
    draft.version = 3
    draft.files[1].caption = "Caption for two"
    draft = try store.update(draft)
    draft = try store.setPhotoSize(draft, size: .small)
    XCTAssertEqual(draft.version, 3)
    let before = draft
    draft = try store.removeFile(draft, file: draft.files[0].id)
    XCTAssertEqual(draft.files.map(\.name), ["two.png"])
    XCTAssertEqual(draft.text, "Caption for two")
    XCTAssertNil(draft.files[0].caption)
    XCTAssertNil(draft.previews)
    XCTAssertFalse(draft.files.contains(where: \.thumbnail))
    XCTAssertThrowsError(try store.update(before))
    draft.files[0].operation!.state = .dispatching
    draft = try store.update(draft)
    XCTAssertThrowsError(try store.removeFile(draft, file: draft.files[0].id))
    draft.files[0].operation!.state = .sent
    draft.files[0].operation!.eventId = "$confirmed"
    draft = try store.update(draft)
    XCTAssertThrowsError(try store.removeFile(draft, file: draft.files[0].id))
  }
  func testCaptionBudgetRecoveryAndContentFences() throws {
    let source = root.appendingPathComponent("photo.png")
    try PhotoFixture.write(source)
    var draft = try store.claim(store.create(text: "First caption").id)
    for _ in 0..<2 {
      draft = try store.importFile(
        draft.id, owner: draft.owner!, source: source, name: "photo.png", mime: "image/png", photo: true)
    }
    var invalid = draft
    invalid.files[1].caption = "Ignored by old readers"
    XCTAssertThrowsError(try store.update(invalid))
    draft.version = 3
    draft.files[1].caption = String(repeating: "é", count: ShareLimits.textBytes / 2)
    XCTAssertEqual(TextShare.validation(draft), "text_too_large")
    XCTAssertThrowsError(try store.update(draft))
    draft.files[1].caption = "Second caption"
    draft = try store.update(draft, release: true)
    draft = try store.claim(draft.id)
    XCTAssertEqual(draft.caption(for: draft.files[1]), "Second caption")
    draft.files[0].operation!.state = .dispatching
    draft = try store.update(draft)
    invalid = draft
    invalid.files[1].caption = "Must not change an uncertain batch"
    XCTAssertThrowsError(try store.update(invalid))
    draft.files[0].operation!.state = .sent
    draft.files[0].operation!.eventId = "$confirmed"
    draft = try store.update(draft)
    invalid = draft
    invalid.files[1].caption = "Must not change a partially delivered batch"
    XCTAssertThrowsError(try store.update(invalid))
  }

  func testSizeChoicesPreserveOriginalsAndRecoveryUploadsTheSelectedDimensions() throws {
    let source = root.appendingPathComponent("photo.heic")
    try PhotoFixture.write(source, type: .heic, width: 2400, height: 1600)
    let originalBytes = try Data(contentsOf: source)
    var draft = try store.claim(store.create(text: "Caption").id)
    for index in 0..<2 {
      draft = try store.importFile(
        draft.id, owner: draft.owner!, source: source,
        name: "photo-\(index).heic", mime: "image/heic", photo: true)
    }
    let ids = draft.files.map(\.id)
    draft = try store.setPhotoSize(draft, size: .small)
    XCTAssertEqual(draft.files.map(\.photoSize), [.small, .small])
    XCTAssertEqual(draft.version, 2)
    draft = try store.setPhotoSize(draft, size: .medium, file: ids[0])
    XCTAssertEqual(draft.files.map(\.photoSize), [.medium, .small])
    draft = try store.update(draft, release: true)
    draft = try store.claim(draft.id)
    for (index, pixels) in [1280, 640].enumerated() {
      try store.withUploadFile(draft.id, file: ids[index], owner: draft.owner!) { url, uploaded in
        XCTAssertEqual(uploaded.mime, "image/jpeg")
        XCTAssertTrue(uploaded.name.hasSuffix(".jpg"))
        let image = try XCTUnwrap(CGImageSourceCreateWithURL(url as CFURL, nil))
        let properties = try XCTUnwrap(
          CGImageSourceCopyPropertiesAtIndex(image, 0, nil) as? [CFString: Any])
        XCTAssertEqual(properties[kCGImagePropertyPixelWidth] as? Int, pixels)
        XCTAssertLessThanOrEqual(
          try XCTUnwrap(properties[kCGImagePropertyPixelHeight] as? Int), pixels)
        XCTAssertEqual(uploaded.bytes, try Data(contentsOf: url).count)
      }
      try store.withVerifiedFile(draft.id, file: ids[index], owner: draft.owner!) { url, _ in
        XCTAssertEqual(try Data(contentsOf: url), originalBytes)
      }
    }
    draft = try store.setPhotoSize(draft, size: .original)
    for id in ids {
      try store.withUploadFile(draft.id, file: id, owner: draft.owner!) { url, file in
        XCTAssertEqual(file.mime, "image/heic")
        XCTAssertEqual(try Data(contentsOf: url), originalBytes)
      }
    }
  }

  func testNoUpscalingAndPNGTransparencyIsPreserved() throws {
    let source = root.appendingPathComponent("photo.png")
    try PhotoFixture.write(source, width: 1800, height: 1200, alpha: true)
    XCTAssertNil(try PhotoShare.resized(source, size: .large))
    let output = try XCTUnwrap(PhotoShare.resized(source, size: .small))
    XCTAssertEqual(output.mime, "image/png")
    let image = try XCTUnwrap(CGImageSourceCreateWithData(output.data as CFData, nil))
    let properties = try XCTUnwrap(
      CGImageSourceCopyPropertiesAtIndex(image, 0, nil) as? [CFString: Any])
    XCTAssertEqual(properties[kCGImagePropertyHasAlpha] as? Bool, true)
    XCTAssertLessThan(output.data.count, ShareLimits.resizedPhotoBytes)
  }

  func testBatchSizeChangeIsAtomicAndFencedByCancellationAndDelivery() throws {
    let source = root.appendingPathComponent("photo.png")
    try PhotoFixture.write(source, width: 1000, height: 800)
    var draft = try store.claim(store.create(text: "").id)
    for index in 0..<2 {
      draft = try store.importFile(
        draft.id, owner: draft.owner!, source: source,
        name: "photo-\(index).png", mime: "image/png", photo: true)
    }
    try FileManager.default.removeItem(
      at: root.appendingPathComponent(draft.id).appendingPathComponent(draft.files[1].id))
    XCTAssertThrowsError(try store.setPhotoSize(draft, size: .small))
    XCTAssertTrue(try store.list()[0].files.allSatisfy { $0.photoSize == nil })
    let cancellation = ShareCancellation()
    cancellation.cancel()
    XCTAssertThrowsError(try store.setPhotoSize(draft, size: .original, cancellation: cancellation))
    draft.files[0].operation!.state = .dispatching
    draft = try store.update(draft)
    XCTAssertThrowsError(try store.setPhotoSize(draft, size: .small))
    draft.files[0].operation!.state = .unknown
    draft = try store.update(draft)
    XCTAssertThrowsError(try store.setPhotoSize(draft, size: .original))
  }

  func testInterruptedResizeUploadDoesNotMarkProviderIntakeIncomplete() throws {
    let draft = try store.claim(store.create(text: "caption").id)
    let directory = root.appendingPathComponent(draft.id)
    try ShareJSON.encode(1024).write(
      to: directory.appendingPathComponent("photo-upload-fixture.part.reservation"))
    try Data("temporary encoding".utf8).write(
      to: directory.appendingPathComponent("photo-upload-fixture.part"))
    let later = try ShareStore(
      root: root, now: { Date().timeIntervalSince1970 + ShareLimits.lease + 1 })
    let recovered = try XCTUnwrap(later.list().first)
    XCTAssertTrue(recovered.importFailures.isEmpty)
    XCTAssertNil(recovered.owner)
    XCTAssertFalse(
      FileManager.default.fileExists(
        atPath: directory.appendingPathComponent("photo-upload-fixture.part").path))
  }

}
