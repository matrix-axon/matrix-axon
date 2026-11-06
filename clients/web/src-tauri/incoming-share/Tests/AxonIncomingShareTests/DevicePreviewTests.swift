#if os(iOS)
  import Foundation
  import ImageIO
  import LinkPresentation
  import UniformTypeIdentifiers
  import UIKit
  import XCTest

  @testable import AxonIncomingShare

  final class DevicePreviewTests: XCTestCase {
    private var root: URL!
    private var store: ShareStore!
    private let link = "https://example.org/article"
    private var png: Data {
      UIGraphicsImageRenderer(size: CGSize(width: 2, height: 2)).image { context in
        UIColor.systemBlue.setFill()
        context.fill(CGRect(x: 0, y: 0, width: 2, height: 2))
      }.pngData()!
    }
    override func setUpWithError() throws {
      root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
      store = try ShareStore(root: root)
      // Local preview acquisition requires staging authority, not credentials.
      try store.publish(
        ShareSession(
          installation: "test", server: "https://axon.example",
          credentialAvailable: false, expiresAt: nil, stagingAllowed: true), credential: {})
    }
    override func tearDownWithError() throws { try FileManager.default.removeItem(at: root) }
    private func draft() throws -> ShareManifest { try store.claim(store.create(text: link).id) }
    private func metadata(title: String? = "Device title", image: NSItemProvider? = nil)
      -> LPLinkMetadata
    {
      let metadata = LPLinkMetadata()
      metadata.originalURL = URL(string: link)
      metadata.url = metadata.originalURL
      metadata.title = title
      metadata.imageProvider = image
      return metadata
    }
    private func provider(_ data: Data, type: UTType = .png) throws -> NSItemProvider {
      let file = root.appendingPathComponent(UUID().uuidString)
      try data.write(to: file)
      let provider = NSItemProvider()
      provider.registerFileRepresentation(
        forTypeIdentifier: type.identifier, fileOptions: [], visibility: .all
      ) {
        completion in
        completion(file, false, nil)
        return nil
      }
      return provider
    }

    func testSuppliedPreviewIsReusedWithoutLookup() async throws {
      var input = try draft()
      input.previews = [SharePreview(matched_url: link, title: "Supplied title")]
      input = try store.update(input)
      let generator = DevicePreview(store: store) { _, _ in
        XCTFail("Supplied metadata must not cause acquisition")
        throw DevicePreviewFailure.unavailable
      }
      let result = try await Task.detached { try generator.include(input, url: self.link) }.value
      XCTAssertTrue(result.reused)
      XCTAssertEqual(result.manifest, input)
    }
    func testSuppliedHTTPPreviewNeedsNoNetworkEligibility() async throws {
      let url = "http://example.org/article"
      var input = try store.claim(store.create(text: url).id)
      input.previews = [SharePreview(matched_url: url, title: "Supplied title")]
      input = try store.update(input)
      let generator = DevicePreview(store: store) { _, _ in
        XCTFail("Supplied metadata must not cause acquisition")
        throw DevicePreviewFailure.unavailable
      }
      let result = try await Task.detached { try generator.include(input, url: url) }.value
      XCTAssertTrue(result.reused)
    }
    func testExplicitDeviceLookupWithoutAccountRetainsImage() async throws {
      let input = try draft()
      let metadata = metadata(image: try provider(png))
      var requests = 0
      let generator = DevicePreview(store: store) { url, _ in
        requests += 1
        XCTAssertEqual(url.absoluteString, self.link)
        return metadata
      }
      XCTAssertEqual(requests, 0)
      let result = try await Task.detached { try generator.include(input, url: self.link) }.value
      XCTAssertEqual(requests, 1)
      XCTAssertNil(result.manifest.accountId)
      XCTAssertEqual(result.image, .ready)
      XCTAssertEqual(result.manifest.previews?.first?.title, "Device title")
      XCTAssertEqual(result.manifest.previews?.first?.provenance, "generated")
      XCTAssertNil(result.manifest.previews?.first?.image_upload_id)
      let file = try XCTUnwrap(result.manifest.previews?.first?.thumbnail)
      XCTAssertEqual(
        try store.readFile(
          result.manifest.id, file: file, owner: result.manifest.owner!, offset: 0, count: 1024),
        png)
      XCTAssertNil(TextShare.validation(result.manifest))
    }
    func testFailedImageRefreshPreservesVerifiedPreviousThumbnail() async throws {
      for provider in [nil, try self.provider(Data("invalid image".utf8))] {
        var input = try draft()
        input = try store.importThumbnail(
          input.id, owner: input.owner!, data: png, mime: "image/png",
          preview: SharePreview(matched_url: link, title: "Original"))
        let file = try XCTUnwrap(input.previews?.first?.thumbnail)
        let metadata = metadata(title: "Refreshed title", image: provider)
        let generator = DevicePreview(store: store) { _, _ in metadata }
        let result = try await Task.detached {
          try generator.include(input, url: self.link, refresh: true)
        }.value
        XCTAssertEqual(result.manifest.previews?.first?.title, "Refreshed title")
        XCTAssertEqual(result.manifest.previews?.first?.thumbnail, file)
        XCTAssertEqual(result.image, .ready)
        XCTAssertEqual(
          try store.readFile(input.id, file: file, owner: input.owner!, offset: 0, count: 1024), png
        )
        XCTAssertNil(TextShare.validation(result.manifest))
      }
    }
    func testRefreshRequiresExplicitRequestAndReplacesOneCard() async throws {
      var input = try draft()
      input.previews = [SharePreview(matched_url: link, title: "Source title")]
      input = try store.update(input)
      let generator = DevicePreview(store: store) { _, _ in self.metadata() }
      let result = try await Task.detached {
        try generator.include(input, url: self.link, refresh: true)
      }.value
      XCTAssertFalse(result.reused)
      XCTAssertEqual(result.image, .absent)
      XCTAssertEqual(result.manifest.previews?.count, 1)
      XCTAssertEqual(result.manifest.previews?.first?.title, "Device title")
    }
    func testFailureDoesNotChangeDraftOrInvokeAnotherService() async throws {
      for failure in [DevicePreviewFailure.timeout, .unavailable, .noMetadata, .publicIPv4Required] {
        let input = try draft()
        var requests = 0
        let generator = DevicePreview(store: store) { _, _ in
          requests += 1
          throw failure
        }
        do {
          _ = try await Task.detached { try generator.include(input, url: self.link) }.value
          XCTFail("Expected nonfatal device fallback")
        } catch { XCTAssertEqual(error as? DevicePreviewFailure, failure) }
        XCTAssertEqual(requests, 1)
        XCTAssertEqual(try store.refreshOwned(input.id, owner: input.owner!), input)
        XCTAssertNil(TextShare.validation(input))
      }
    }
    func testMissingPublicIPv4ExplainsTheRestrictionAndKeepsTheDraft() async throws {
      let input = try draft()
      let generator = DevicePreview(store: store) { _, _ in
        throw ShareFailure.publicIPv4Required
      }
      do {
        _ = try await Task.detached { try generator.include(input, url: self.link) }.value
        XCTFail("An IPv6-only origin cannot start preview acquisition")
      } catch {
        XCTAssertEqual(error as? DevicePreviewFailure, .publicIPv4Required)
        XCTAssertTrue(DevicePreviewFailure.publicIPv4Required.explanation.contains("IPv6-only"))
      }
      XCTAssertEqual(try store.refreshOwned(input.id, owner: input.owner!), input)
      XCTAssertNil(TextShare.validation(input))
    }
    func testMissingAndOversizedImagesPreserveTextCard() async throws {
      for image in [
        try provider(Data(repeating: 0, count: ShareLimits.thumbnailBytes + 1)),
        try provider(Data("invalid".utf8)),
      ] {
        let input = try draft()
        let metadata = metadata(image: image)
        let generator = DevicePreview(store: store) { _, _ in metadata }
        let result = try await Task.detached { try generator.include(input, url: self.link) }.value
        XCTAssertEqual(result.image, .unavailable)
        XCTAssertEqual(result.manifest.previews?.first?.title, "Device title")
        XCTAssertTrue(result.manifest.files.isEmpty)
        XCTAssertNil(TextShare.validation(result.manifest))
      }
    }
    func testEmptyAndMismatchedMetadataDoNotReplaceDraft() async throws {
      for mismatch in [false, true] {
        let input = try draft()
        let metadata = metadata(title: mismatch ? "Wrong target" : nil)
        if mismatch { metadata.originalURL = URL(string: "https://other.example/") }
        let generator = DevicePreview(store: store) { _, _ in metadata }
        do {
          _ = try await Task.detached { try generator.include(input, url: self.link) }.value
          XCTFail("Unusable metadata must not be published")
        } catch { XCTAssertEqual(error as? DevicePreviewFailure, .noMetadata) }
        XCTAssertEqual(try store.refreshOwned(input.id, owner: input.owner!), input)
      }
    }
    func testCancellationAndLostSessionFencePublication() async throws {
      let input = try draft()
      let cancellation = ShareCancellation()
      let generator = DevicePreview(store: store, cancellation: cancellation) { _, token in
        token.cancel()
        return self.metadata()
      }
      do {
        _ = try await Task.detached { try generator.include(input, url: self.link) }.value
        XCTFail("Cancelled result must not be published")
      } catch { XCTAssertEqual(error as? ShareFailure, .cancelled) }
      XCTAssertEqual(try store.refreshOwned(input.id, owner: input.owner!), input)
      let invalidating = DevicePreview(store: store) { _, _ in
        try self.store.invalidateSession()
        return self.metadata()
      }
      do {
        _ = try await Task.detached { try invalidating.include(input, url: self.link) }.value
        XCTFail("Signed-out result must not be published")
      } catch {}
      XCTAssertTrue(try store.list().isEmpty)
    }
    func testUnsafeAddressAndStaleRevisionAreRejectedBeforeLookup() async throws {
      for raw in [
        "https://localhost/a", "https://127.0.0.1/a", "https://router.local./a", "https://[::1]/a",
        "https://user:pass@example.org/a", "file:///private/a",
        "https://router.lan/a", "https://service.corp/a", "https://router.home/a",
        "https://router.localdomain/a",
      ] {
        XCTAssertFalse(DevicePreview.permits(raw))
      }
      let input = try draft()
      _ = try store.update(input)
      let generator = DevicePreview(store: store) { _, _ in
        XCTFail("Stale owner must not start acquisition")
        return self.metadata()
      }
      do {
        _ = try await Task.detached { try generator.include(input, url: self.link) }.value
        XCTFail("Expected stale revision")
      } catch { XCTAssertEqual(error as? ShareFailure, .stale) }
    }
    func testGenericPublicImageIsConvertedToStagingFormat() async throws {
      let source = try ShareStore.checkedImageSource(png)
      let bitmap = try XCTUnwrap(CGImageSourceCreateImageAtIndex(source, 0, nil))
      let tiff = NSMutableData()
      let destination = try XCTUnwrap(
        CGImageDestinationCreateWithData(tiff, UTType.tiff.identifier as CFString, 1, nil))
      CGImageDestinationAddImage(destination, bitmap, nil)
      XCTAssertTrue(CGImageDestinationFinalize(destination))
      let input = try draft()
      let metadata = metadata(image: try provider(tiff as Data, type: .image))
      let generator = DevicePreview(store: store) { _, _ in metadata }
      let result = try await Task.detached { try generator.include(input, url: self.link) }.value
      XCTAssertEqual(result.image, .ready)
      XCTAssertEqual(result.manifest.files.first?.mime, "image/jpeg")
    }
    func testResultWaitIsCancellableAndIgnoresLateCallbacks() throws {
      let result = ShareIntake.ResultBox<String>()
      let cancellation = ShareCancellation()
      cancellation.cancel()
      XCTAssertThrowsError(try result.wait(cancellation: cancellation)) {
        XCTAssertEqual($0 as? ShareFailure, .cancelled)
      }
      result.finish(.success("late"))
      XCTAssertNil(result.value)
    }
  }
#endif
