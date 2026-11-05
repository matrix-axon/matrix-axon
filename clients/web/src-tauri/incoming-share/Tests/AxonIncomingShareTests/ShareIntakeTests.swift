#if os(iOS)
  import Foundation
  import LinkPresentation
  import Network
  import UIKit
  import UniformTypeIdentifiers
  import XCTest

  @testable import AxonIncomingShare

  final class ShareIntakeTests: XCTestCase {
    private final class MetadataAndURLProvider: NSItemProvider {
      let metadata: LPLinkMetadata
      init(metadata: LPLinkMetadata) {
        self.metadata = metadata
        super.init()
        registerObject(metadata.originalURL! as NSURL, visibility: .all)
      }
      override func loadItem(forTypeIdentifier typeIdentifier: String,
                             options: [AnyHashable: Any]? = nil,
                             completionHandler: NSItemProvider.CompletionHandler? = nil) {
        completionHandler?(metadata, nil)
      }
    }
    private final class StringProvider: NSItemProvider {
      var offeredFile: URL?
      override func loadFileRepresentation(
        forTypeIdentifier typeIdentifier: String,
        completionHandler: @escaping (URL?, Error?) -> Void
      ) -> Progress {
        completionHandler(
          offeredFile, offeredFile == nil ? NSError(domain: "synthetic", code: 1) : nil)
        return Progress(totalUnitCount: 1)
      }
    }
    private var root: URL!
    private var store: ShareStore!
    private let link = "https://example.org/"
    private var png: Data {
      Data(
        base64Encoded:
          "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aH1sAAAAASUVORK5CYII="
      )!
    }
    override func setUpWithError() throws {
      root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
      store = try ShareStore(root: root)
      try store.publish(
        ShareSession(
          installation: "test", server: "https://axon.example", credentialAvailable: true,
          expiresAt: nil), credential: {})
    }
    override func tearDownWithError() throws { try FileManager.default.removeItem(at: root) }
    func testSuppliedPublicMetadataWinsOverAnAvailableURLReader() async throws {
      let metadata = LPLinkMetadata()
      metadata.originalURL = URL(string: link)
      metadata.title = "Source title"
      let image = NSItemProvider()
      let data = png
      image.registerDataRepresentation(forTypeIdentifier: UTType.png.identifier, visibility: .all) {
        completion in
        completion(data, nil)
        return nil
      }
      metadata.imageProvider = image
      let provider = MetadataAndURLProvider(metadata: metadata)
      XCTAssertTrue(provider.canLoadObject(ofClass: NSURL.self))
      let item = NSExtensionItem()
      item.attachments = [provider]
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.text, link)
      XCTAssertEqual(result.previews?.first?.title, "Source title")
      XCTAssertNotNil(result.previews?.first?.thumbnail)
      XCTAssertTrue(result.importFailures.isEmpty)
    }
    func testCancelledIntakeNeverCreatesOrClaimsADraft() throws {
      let cancellation = ShareCancellation()
      cancellation.cancel()
      let item = NSExtensionItem()
      item.attachments = [NSItemProvider(object: URL(string: link)! as NSURL)]
      XCTAssertThrowsError(try ShareIntake.importItems([item], store: store, cancellation: cancellation)) {
        XCTAssertEqual($0 as? ShareFailure, .cancelled)
      }
      XCTAssertTrue(try store.list().isEmpty)
    }
    func testCancellationInterruptsAProviderBeforeItCanPublishADraft() async throws {
      let cancellation = ShareCancellation()
      let started = expectation(description: "provider read started")
      let provider = NSItemProvider()
      provider.registerDataRepresentation(forTypeIdentifier: UTType.url.identifier, visibility: .all) { _ in
        started.fulfill()
        return Progress(totalUnitCount: 1)
      }
      let item = NSExtensionItem()
      item.attachments = [provider]
      let task = Task.detached {
        try ShareIntake.importItems([item], store: self.store, cancellation: cancellation)
      }
      await fulfillment(of: [started], timeout: 3)
      cancellation.cancel()
      switch await task.result {
      case .success: XCTFail("Cancelled provider must not publish a draft")
      case .failure(let error): XCTAssertEqual(error as? ShareFailure, .cancelled)
      }
      XCTAssertTrue(try store.list().isEmpty)
    }
    func testPublicURLAndAccompanyingTextImportOnceWithoutFetching() async throws {
      let item = NSExtensionItem()
      item.attributedContentText = NSAttributedString(string: "Useful accompanying prose " + link)
      item.attachments = [NSItemProvider(object: URL(string: link)! as NSURL)]
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.text, "Useful accompanying prose " + link)
      XCTAssertEqual(result.operation.state, .ready)
      XCTAssertTrue(result.importFailures.isEmpty, result.importFailures.joined(separator: ","))
      XCTAssertNil(result.previews)
    }
    func testSystemStringProvider() async throws {
      let item = NSExtensionItem()
      item.attachments = [NSItemProvider(object: "Substantive text 😀" as NSString)]
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.text, "Substantive text 😀")
      XCTAssertTrue(result.importFailures.isEmpty)
    }
    func testStringOnlyProviderAndAttributedURLImportWithoutDuplicateOrFailure() async throws {
      let item = NSExtensionItem()
      item.attributedContentText = NSAttributedString(string: link)
      item.attachments = [StringProvider(object: link as NSString)]
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.text, link)
      XCTAssertTrue(result.importFailures.isEmpty)
      XCTAssertNil(TextShare.validation(result))
    }
    func testStringOnlyProviderPreservesAccompanyingProse() async throws {
      let item = NSExtensionItem()
      item.attributedContentText = NSAttributedString(string: link)
      item.attachments = [StringProvider(object: "A substantive note \(link)" as NSString)]
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.text, "A substantive note \(link)")
      XCTAssertTrue(result.importFailures.isEmpty)
    }
    func testStringFallbackStillRejectsOversizedText() async throws {
      let item = NSExtensionItem()
      item.attributedContentText = NSAttributedString(string: link)
      item.attachments = [StringProvider(object: String(repeating: "😀", count: 20_000) as NSString)]
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.text, link)
      XCTAssertEqual(result.importFailures, ["quota"])
    }
    func testOversizedFileCannotFallBackToSmallerStringAlternative() async throws {
      let file = root.appendingPathComponent("oversized.txt")
      try Data(repeating: 65, count: ShareLimits.textBytes + 1).write(to: file)
      let provider = StringProvider(object: link as NSString)
      provider.offeredFile = file
      let item = NSExtensionItem()
      item.attributedContentText = NSAttributedString(string: link)
      item.attachments = [provider]
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.importFailures, ["quota"])
    }
    func testInvalidUTF8FileCanUsePublicStringRepresentation() async throws {
      let file = root.appendingPathComponent("invalid.txt")
      try Data([255, 255]).write(to: file)
      let provider = StringProvider(object: link as NSString)
      provider.offeredFile = file
      let item = NSExtensionItem()
      item.attachments = [provider]
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.text, link)
      XCTAssertTrue(result.importFailures.isEmpty)
    }
    func testConcreteUTF16TextRepresentation() async throws {
      let value = "A substantive note 😀 \(link)"
      let file = root.appendingPathComponent("utf16.txt")
      try XCTUnwrap(value.data(using: .utf16)).write(to: file)
      let provider = NSItemProvider()
      provider.registerFileRepresentation(
        forTypeIdentifier: UTType.utf16PlainText.identifier, fileOptions: [], visibility: .all
      ) { completion in
        completion(file, false, nil)
        return nil
      }
      let item = NSExtensionItem()
      item.attachments = [provider]
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.text, value)
      XCTAssertTrue(result.importFailures.isEmpty)
    }
    func testLegacyURLRegisteredAsPlainTextUsesPublicObjectWithoutDecodingArchive() async throws {
      let url = URL(string: link)! as NSURL
      let provider = NSItemProvider()
      provider.registerItem(forTypeIdentifier: UTType.plainText.identifier) { completion, _, _ in
        completion?(url, nil)
      }
      let item = NSExtensionItem()
      item.attributedContentText = NSAttributedString(string: link)
      item.attachments = [provider]
      XCTAssertFalse(provider.hasItemConformingToTypeIdentifier(UTType.url.identifier))
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.text, link)
      XCTAssertTrue(result.importFailures.isEmpty)
    }
    func testLegacyWebURLImportDoesNotFetchSource() async throws {
      let queue = DispatchQueue(label: "share-intake-network-test")
      let ready = expectation(description: "Listener ready")
      let probe = expectation(description: "Listener accepts a control connection")
      let fetched = expectation(description: "Source URL must not be fetched")
      fetched.isInverted = true
      let listener = try NWListener(using: .tcp)
      // Owned by queue, including the transition after the control connection.
      var importing = false
      listener.stateUpdateHandler = { if case .ready = $0 { ready.fulfill() } }
      listener.newConnectionHandler = { connection in
        if importing { fetched.fulfill() } else { probe.fulfill() }
        connection.cancel()
      }
      listener.start(queue: queue)
      defer { listener.cancel() }
      await fulfillment(of: [ready], timeout: 3)
      let port = try XCTUnwrap(listener.port)
      let control = NWConnection(host: "127.0.0.1", port: port, using: .tcp)
      control.start(queue: queue)
      await fulfillment(of: [probe], timeout: 3)
      control.cancel()
      queue.sync { importing = true }
      let url = URL(string: "https://127.0.0.1:\(port.rawValue)/must-not-fetch")! as NSURL
      let provider = NSItemProvider()
      provider.registerItem(forTypeIdentifier: UTType.plainText.identifier) { completion, _, _ in
        completion?(url, nil)
      }
      let item = NSExtensionItem()
      item.attachments = [provider]
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.text, url.absoluteString)
      XCTAssertTrue(result.importFailures.isEmpty)
      await fulfillment(of: [fetched], timeout: 0.5)
    }
    func testLegacyFileURLRegisteredAsPlainTextIsNotImported() async throws {
      let provider = NSItemProvider()
      provider.registerItem(forTypeIdentifier: UTType.plainText.identifier) { completion, _, _ in
        completion?(self.root.appendingPathComponent("private.txt") as NSURL, nil)
      }
      let item = NSExtensionItem()
      item.attributedContentText = NSAttributedString(string: link)
      item.attachments = [provider]
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.text, link)
      XCTAssertFalse(result.importFailures.isEmpty)
    }
    func testPublicTextFileAndTitleMetadata() async throws {
      let url = root.appendingPathComponent("text.txt")
      try Data("A substantive note ".appending(link).utf8).write(to: url)
      let provider = NSItemProvider()
      provider.registerFileRepresentation(
        forTypeIdentifier: UTType.plainText.identifier, fileOptions: [], visibility: .all
      ) { completion in
        completion(url, false, nil)
        return nil
      }
      let item = NSExtensionItem()
      item.attributedTitle = NSAttributedString(string: "Source title")
      item.attachments = [provider, NSItemProvider(object: URL(string: link)! as NSURL)]
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.text, "A substantive note " + link)
      XCTAssertEqual(result.previews?.first?.title, "Source title")
      XCTAssertTrue(result.importFailures.isEmpty, result.importFailures.joined(separator: ","))
    }
    func testSupportedPublicMetadataAndImageAreRetainedLocally() async throws {
      let data = png
      let image = root.appendingPathComponent("image.png")
      try data.write(to: image)
      let provider = NSItemProvider(object: URL(string: link)! as NSURL)
      provider.registerFileRepresentation(
        forTypeIdentifier: UTType.png.identifier, fileOptions: [], visibility: .all
      ) { completion in
        completion(image, false, nil)
        return nil
      }
      let item = NSExtensionItem()
      item.attributedTitle = NSAttributedString(string: "<b>Supplied as plain text</b>")
      item.attachments = [provider]
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.previews?.first?.title, "<b>Supplied as plain text</b>")
      XCTAssertEqual(result.files.count, 1)
      let file = try XCTUnwrap(result.files.first)
      XCTAssertTrue(file.thumbnail)
      XCTAssertNil(file.operation)
      XCTAssertNil(result.previews?.first?.image_upload_id)
      XCTAssertEqual(
        try store.readFile(
          result.id, file: file.id, owner: result.owner!, offset: 0, count: 1024), data)
    }
    func testSeparateImageAttachmentIsRejectedRatherThanInferredAsThumbnail() async throws {
      let image = NSItemProvider()
      let data = png
      image.registerDataRepresentation(forTypeIdentifier: UTType.png.identifier, visibility: .all) {
        completion in
        completion(data, nil)
        return nil
      }
      let item = NSExtensionItem()
      item.attachments = [NSItemProvider(object: URL(string: link)! as NSURL), image]
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.text, link)
      XCTAssertEqual(result.importFailures, ["unsupported_input"])
      XCTAssertTrue(result.files.isEmpty)
      XCTAssertNil(result.previews)
    }
    func testOversizedOptionalSourceTitleNeverBlocksTextSending() async throws {
      for metadataTitle in [false, true] {
        let item = NSExtensionItem()
        let title = String(repeating: "🧪", count: 301)
        if metadataTitle {
          let metadata = LPLinkMetadata()
          metadata.originalURL = URL(string: link)
          metadata.title = title
          let provider = NSItemProvider()
          provider.registerItem(forTypeIdentifier: UTType.url.identifier) { completion, _, _ in
            completion?(metadata, nil)
          }
          item.attachments = [provider]
        } else {
          item.attributedTitle = NSAttributedString(string: title)
          item.attachments = [NSItemProvider(object: URL(string: link)! as NSURL)]
        }
        let result = try await Task.detached {
          try ShareIntake.importItems([item], store: self.store)
        }.value
        XCTAssertEqual(result.text, link)
        XCTAssertTrue(result.importFailures.isEmpty)
        XCTAssertNil(TextShare.validation(result))
      }
    }
    func testUnsupportedURLRepresentationsKeepAccompanyingText() async throws {
      for raw in ["mailto:test@example.org", "file:///private/fixture.txt", "custom:fixture"] {
        let metadata = LPLinkMetadata()
        metadata.originalURL = URL(string: raw)
        for value: NSSecureCoding in [
          URL(string: raw)! as NSURL, raw as NSString, Data(raw.utf8) as NSData, metadata,
        ] {
          let item = NSExtensionItem()
          item.attributedContentText = NSAttributedString(string: "Keep this text")
          let provider = NSItemProvider()
          provider.registerItem(forTypeIdentifier: UTType.url.identifier) { completion, _, _ in
            completion?(value, nil)
          }
          item.attachments = [provider]
          let result = try await Task.detached {
            try ShareIntake.importItems([item], store: self.store)
          }.value
          XCTAssertEqual(result.text, "Keep this text")
          XCTAssertEqual(result.importFailures, ["unsupported_url"])
        }
      }
    }
    func testUnrecognizedImageFallsBackToTextCard() async throws {
      let image = root.appendingPathComponent("image.png")
      try Data("Not an image".utf8).write(to: image)
      let metadata = LPLinkMetadata()
      metadata.originalURL = URL(string: link)
      metadata.title = "Text survives"
      let suppliedImage = NSItemProvider()
      suppliedImage.registerFileRepresentation(
        forTypeIdentifier: UTType.png.identifier, fileOptions: [], visibility: .all
      ) { completion in
        completion(image, false, nil)
        return nil
      }
      metadata.imageProvider = suppliedImage
      let provider = NSItemProvider()
      provider.registerItem(forTypeIdentifier: UTType.url.identifier) { completion, _, _ in
        completion?(metadata, nil)
      }
      let item = NSExtensionItem()
      item.attachments = [provider]
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.previews?.first?.title, "Text survives")
      XCTAssertTrue(result.files.isEmpty)
      XCTAssertNil(TextShare.validation(result))
    }
    func testProviderFailureRetainsOtherContentAndPreventsSilentPartialSend() async throws {
      let provider = NSItemProvider()
      provider.registerFileRepresentation(
        forTypeIdentifier: UTType.plainText.identifier, fileOptions: [], visibility: .all
      ) { completion in
        completion(nil, false, NSError(domain: "synthetic", code: 1))
        return nil
      }
      let item = NSExtensionItem()
      item.attributedContentText = NSAttributedString(string: "Retained text")
      item.attachments = [provider]
      let result = try await Task.detached {
        try ShareIntake.importItems([item], store: self.store)
      }.value
      XCTAssertEqual(result.text, "Retained text")
      XCTAssertEqual(result.importFailures, ["unavailable"])
      XCTAssertEqual(result.operation.state, .ready)
    }
  }
#endif
