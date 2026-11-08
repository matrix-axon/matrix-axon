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

  func testPublishedPreviewStillCountsAgainstPendingQuotaAfterReservationRemoval() throws {
    let source = root.appendingPathComponent("source.png")
    try PhotoFixture.write(source)
    var draft = try store.claim(store.create(text: "").id)
    draft = try store.importFile(draft.id, owner: draft.owner!, source: source, name: "photo.png", mime: "image/png", photo: true)
    let directory = root.appendingPathComponent(draft.id)
    let cache = directory.appendingPathComponent(draft.files[0].id + ".preview")
    XCTAssertTrue(FileManager.default.fileExists(atPath: cache.path))
    XCTAssertFalse(try FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: nil).contains { $0.pathExtension == "reservation" })
    let filler = directory.appendingPathComponent("retained-orphan")
    try Data().write(to: filler)
    let handle = try FileHandle(forWritingTo: filler)
    // Without the published cache there is exactly room for one more record.
    let bytes = ShareLimits.pendingBytes - 16 * 1024 - 2 * ShareLimits.metadataReservation - draft.bytes
    try handle.truncate(atOffset: UInt64(bytes))
    try handle.close()
    XCTAssertThrowsError(try store.create(text: "")) { XCTAssertEqual($0 as? ShareFailure, .quota) }
    try FileManager.default.removeItem(at: cache)
    XCTAssertNoThrow(try store.create(text: ""))
  }

  func testInspectionDownsamplesAHighResolutionPanorama() throws {
    let source = root.appendingPathComponent("panorama.png")
    try PhotoFixture.write(source, width: 60000, height: 32)
    let original = try Data(contentsOf: source)
    let inspected = try PhotoShare.inspect(source)
    XCTAssertEqual(inspected.mime, "image/png")
    let thumbnail = try XCTUnwrap(CGImageSourceCreateWithData(inspected.thumbnail as CFData, nil))
    let image = try XCTUnwrap(CGImageSourceCreateImageAtIndex(thumbnail, 0, nil))
    XCTAssertLessThanOrEqual(image.width, 480)
    XCTAssertLessThanOrEqual(image.height, 480)
    XCTAssertEqual(try Data(contentsOf: source), original)
  }

  func testWhitespaceOnlyLegacyTextDoesNotAdoptAPhotoOutcome() throws {
    let source = root.appendingPathComponent("source.png")
    try PhotoFixture.write(source)
    var draft = try store.claim(store.create(text: "").id)
    draft = try store.importFile(draft.id, owner: draft.owner!, source: source, name: "photo.png", mime: "image/png", photo: true)
    for version in 1...3 {
      for state: DeliveryState in [.sent, .dispatching, .unknown] {
        var legacy = draft
        legacy.version = version
        legacy.text = " \t\n"
        legacy.files[0].caption = nil
        legacy.files[0].operation!.state = state
        legacy.files[0].operation!.eventId = state == .sent ? "$photo" : nil
        let current = legacy.canonicalized()
        XCTAssertFalse(current.hasMessage)
        XCTAssertEqual(current.operation, legacy.operation)
        XCTAssertEqual(current.deliveryOperations.count, 1)
        XCTAssertEqual(current.files[0].operation, legacy.files[0].operation)
      }
    }
  }

  func testSizeChangeRejectsAnImpossibleKnownBudgetBeforeOpeningTargets() throws {
    let source = root.appendingPathComponent("source.png")
    try PhotoFixture.write(source)
    var draft = try store.claim(store.create(text: "").id)
    for name in ["first.png", "second.png"] {
      draft = try store.importFile(draft.id, owner: draft.owner!, source: source, name: name, mime: "image/png", photo: true)
    }
    // A bounded legacy estimate can fill the budget before the selected photo
    // is opened. Preserve the original manifest if preflight rejects it.
    draft.files[1].photoSize = .large
    draft.files[1].photoBytes = ShareLimits.batchBytes
    let directory = root.appendingPathComponent(draft.id)
    try ShareJSON.encode(draft).write(to: directory.appendingPathComponent("manifest.json"))
    try FileManager.default.removeItem(at: directory.appendingPathComponent(draft.files[0].id))
    XCTAssertThrowsError(try store.setPhotoSize(draft, size: .small, file: draft.files[0].id)) {
      XCTAssertEqual($0 as? ShareFailure, .quota)
    }
    XCTAssertEqual(try store.list().first, draft)
  }

  func testRenamePreservesPhotoIdentityAndRecoveryAndFencesDelivery() throws {
    let source = root.appendingPathComponent("source.png")
    try PhotoFixture.write(source)
    var draft = try store.claim(store.create(text: "Source message").id)
    draft = try store.importFile(draft.id, owner: draft.owner!, source: source, name: "original.png", mime: "image/png", photo: true)
    draft.files[0].caption = "Per-photo caption"
    draft = try store.update(draft)
    let original = draft
    let file = draft.files[0]
    let preview = try store.photoThumbnail(draft.id, file: file.id, owner: draft.owner!)
    for invalid in ["", "  ", "../other.png", "bad\nname.png", String(repeating: "é", count: 128)] {
      XCTAssertFalse(ShareStore.validPhotoName(invalid))
      XCTAssertThrowsError(try store.renameFile(draft, file: file.id, name: invalid))
    }
    XCTAssertThrowsError(try store.renameFile(draft, file: UUID().uuidString, name: "other.png"))
    draft = try store.renameFile(draft, file: file.id, name: "  Café coast.png  ")
    var renamed = file
    renamed.name = "Café coast.png"
    XCTAssertEqual(draft.files, [renamed])
    XCTAssertEqual(draft.text, original.text)
    XCTAssertEqual(try store.photoThumbnail(draft.id, file: file.id, owner: draft.owner!), preview)
    XCTAssertThrowsError(try store.renameFile(original, file: file.id, name: "stale.png"))
    try store.withUploadFile(draft.id, file: file.id, owner: draft.owner!) { _, upload in
      XCTAssertEqual(upload.name, "Café coast.png")
      XCTAssertEqual(upload.digest, file.digest)
    }
    draft = try store.update(draft, release: true)
    draft = try store.claim(draft.id)
    XCTAssertEqual(draft.files, [renamed])
    draft.files[0].operation!.state = .dispatching
    draft = try store.update(draft)
    XCTAssertThrowsError(try store.renameFile(draft, file: file.id, name: "late.png"))
    draft.files[0].operation!.state = .unknown
    draft = try store.update(draft)
    XCTAssertThrowsError(try store.renameFile(draft, file: file.id, name: "uncertain.png"))
  }

  func testReorderPreservesContentAndFencesDelivery() throws {
    let source = root.appendingPathComponent("source.png")
    try PhotoFixture.write(source)
    var draft = try store.claim(store.create(text: "Source message").id)
    for name in ["first.png", "second.png"] {
      draft = try store.importFile(draft.id, owner: draft.owner!, source: source, name: name, mime: "image/png", photo: true)
    }
    draft.files[0].caption = "First 🌊"
    draft.files[1].caption = "Second"
    draft = try store.update(draft)
    let original = draft
    let reversed = Array(draft.files.map(\.id).reversed())
    XCTAssertThrowsError(try store.reorderFiles(draft, order: [reversed[0], reversed[0]]))
    XCTAssertThrowsError(try store.reorderFiles(draft, order: [UUID().uuidString, reversed[1]]))
    draft = try store.reorderFiles(draft, order: reversed)
    XCTAssertEqual(draft.files, Array(original.files.reversed()))
    XCTAssertEqual(draft.text, original.text)
    XCTAssertEqual(draft.previews, original.previews)
    XCTAssertThrowsError(try store.reorderFiles(original, order: reversed))
    draft = try store.update(draft, release: true)
    draft = try store.claim(draft.id)
    XCTAssertEqual(draft.files.map(\.id), reversed)
    draft.files[0].operation!.state = .dispatching
    draft = try store.update(draft)
    XCTAssertThrowsError(try store.reorderFiles(draft, order: Array(reversed.reversed())))
    draft.files[0].operation!.state = .unknown
    draft = try store.update(draft)
    XCTAssertThrowsError(try store.reorderFiles(draft, order: reversed))
  }

  func testOriginalNamesAndCachedPreviewsDoNotReadOriginalsAgain() throws {
    let source = root.appendingPathComponent("source.png")
    try PhotoFixture.write(source, width: 1800, height: 1200)
    var draft = try store.claim(store.create(text: "").id)
    draft = try store.importFile(draft.id, owner: draft.owner!, source: source, name: "IMG_1234", mime: "image/*", photo: true)
    let file = draft.files[0]
    XCTAssertEqual(file.name, "IMG_1234.png")
    let thumbnail = try store.photoThumbnail(draft.id, file: file.id, owner: draft.owner!)
    let retained = root.appendingPathComponent(draft.id).appendingPathComponent(file.id)
    try Data("corrupted original".utf8).write(to: retained)
    XCTAssertEqual(try store.photoThumbnail(draft.id, file: file.id, owner: draft.owner!), thumbnail)
    draft = try store.update(draft, release: true)
    draft = try store.claim(draft.id)
    XCTAssertEqual(try store.photoThumbnail(draft.id, file: file.id, owner: draft.owner!), thumbnail)
    XCTAssertThrowsError(try store.withUploadFile(draft.id, file: file.id, owner: draft.owner!) { _, _ in })
  }

  func testBatchSizingLeavesAnimationOriginalAndResizesStillPhotos() throws {
    let source = root.appendingPathComponent("source.png")
    try PhotoFixture.write(source, width: 1800, height: 1200)
    let animation = root.appendingPathComponent("animation.gif")
    let image = try XCTUnwrap(CGImageSourceCreateWithURL(source as CFURL, nil))
    let frame = try XCTUnwrap(CGImageSourceCreateImageAtIndex(image, 0, nil))
    let destination = try XCTUnwrap(CGImageDestinationCreateWithURL(animation as CFURL, UTType.gif.identifier as CFString, 2, nil))
    CGImageDestinationAddImage(destination, frame, nil)
    CGImageDestinationAddImage(destination, frame, nil)
    XCTAssertTrue(CGImageDestinationFinalize(destination))
    var draft = try store.claim(store.create(text: "").id)
    for (url, name) in [(source, "still"), (animation, "animated")] {
      draft = try store.importFile(draft.id, owner: draft.owner!, source: url, name: name, mime: "image/*", photo: true)
    }
    draft = try store.setPhotoSize(draft, size: .small)
    XCTAssertEqual(draft.files.map(\.photoSize), [.small, .original])
    XCTAssertLessThan(draft.files[0].uploadBytes, draft.files[0].bytes)
    XCTAssertEqual(draft.files[1].uploadBytes, draft.files[1].bytes)
    XCTAssertThrowsError(try store.setPhotoSize(draft, size: .small, file: draft.files[1].id))
    XCTAssertEqual(try store.refreshOwned(draft.id, owner: draft.owner!).files.map(\.photoSize), [.small, .original])
  }

  func testLegacyDraftUpgradePreservesIndependentSourceAndDeliveryFences() throws {
    let source = root.appendingPathComponent("source.png")
    try PhotoFixture.write(source)
    for version in [1, 2, 3] {
      for state in [DeliveryState.ready, .sent, .unknown] {
        var draft = try store.claim(store.create(text: "Shared message https://example.org").id)
        for _ in 0..<2 {
          draft = try store.importFile(draft.id, owner: draft.owner!, source: source, name: "photo", mime: "image/*", photo: true)
        }
        draft.version = version
        draft.files[0].name = "IMG_OLD"
        draft.files[0].caption = nil
        draft.files[0].operation!.state = state
        draft.files[0].operation!.eventId = state == .sent ? "$legacy" : nil
        draft.files[1].caption = version == 3 ? "Second photo" : nil
        try JSONEncoder().encode(draft).write(to: root.appendingPathComponent(draft.id).appendingPathComponent("manifest.json"))
        let upgraded = try store.refreshOwned(draft.id, owner: draft.owner!)
        XCTAssertEqual(upgraded.version, 4)
        XCTAssertEqual(upgraded.text, draft.text)
        XCTAssertEqual(upgraded.files.map(\.caption), ["", version == 3 ? "Second photo" : ""])
        XCTAssertEqual(upgraded.operation.state, state)
        XCTAssertEqual(upgraded.operation.eventId, state == .sent ? "$legacy" : nil)
        XCTAssertEqual(upgraded.files[0].operation?.state, state)
        if state == .ready {
          let name = try store.withUploadFile(upgraded.id, file: upgraded.files[0].id, owner: upgraded.owner!) { _, file in file.name }
          XCTAssertEqual(name, "IMG_OLD.png")
        }
      }
    }
  }

  func testRemovingEveryPhotoRetainsSourceMessageAndCards() throws {
    let source = root.appendingPathComponent("source.png")
    try PhotoFixture.write(source)
    let text = "Shared message https://example.org"
    var draft = try store.claim(store.create(text: text).id)
    draft = try store.importFile(draft.id, owner: draft.owner!, source: source, name: "photo", mime: "image/*", photo: true)
    draft = try store.importThumbnail(draft.id, owner: draft.owner!, data: Data(contentsOf: source), mime: "image/png", preview: SharePreview(matched_url: "https://example.org", title: "Source card"))
    draft = try store.removeFile(draft, file: draft.files.first(where: { !$0.thumbnail })!.id)
    XCTAssertEqual(draft.text, text)
    XCTAssertEqual(draft.previews?.first?.title, "Source card")
    XCTAssertEqual(draft.files.filter { !$0.thumbnail }.count, 0)
    XCTAssertEqual(draft.files.filter(\.thumbnail).count, 1)
    XCTAssertNil(TextShare.validation(draft))
  }

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
    draft.files[1].caption = "Caption for two"
    draft = try store.update(draft)
    draft = try store.setPhotoSize(draft, size: .small)
    XCTAssertEqual(draft.version, 4)
    let before = draft
    draft = try store.removeFile(draft, file: draft.files[0].id)
    XCTAssertEqual(draft.files.filter { !$0.thumbnail }.map(\.name), ["two.png"])
    XCTAssertEqual(draft.text, "Caption")
    XCTAssertEqual(draft.files[0].caption, "Caption for two")
    XCTAssertEqual(draft.previews?.count, 1)
    XCTAssertTrue(draft.files.contains(where: \.thumbnail))
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
    invalid.version = 3
    invalid.files[1].caption = "Ignored by old readers"
    XCTAssertThrowsError(try store.update(invalid))
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
    XCTAssertEqual(draft.version, 4)
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
    try ShareJSON.encode(1024).write(to: directory.appendingPathComponent("photo-preview-fixture.part.reservation"))
    try Data("temporary preview".utf8).write(to: directory.appendingPathComponent("photo-preview-fixture.part"))
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
