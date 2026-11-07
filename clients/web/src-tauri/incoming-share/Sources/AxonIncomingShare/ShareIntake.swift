#if os(iOS)
  import Darwin
  import Foundation
  import LinkPresentation
  import UIKit
  import UniformTypeIdentifiers
  import os

  /// Public representations only. Never decode an archive or ask LinkPresentation
  /// to fetch a page. Provider callbacks are bounded and late callbacks are ignored.
  public enum ShareIntake {
    private static let log = Logger(subsystem: "org.matrixaxon.axon", category: "share-intake")
    private struct SourceMetadata {
      var url: String
      var title: String?
      var imageProvider: NSItemProvider?
    }
    typealias ResultBox<T> = ShareIntakeResult<T>

    private static func item(
      _ provider: NSItemProvider, type: String, cancellation: ShareCancellation
    ) throws -> NSSecureCoding {
      // The public coercing interface may supply LPLinkMetadata even when a
      // typed NSURL reader is also available. Preserve that metadata first;
      // asking for an NSSecureCoding object does not request URL contents.
      let box = ResultBox<NSSecureCoding>()
      provider.loadItem(forTypeIdentifier: type, options: nil) { value, _ in
        if let value {
          box.finish(.success(value))
        } else {
          box.finish(.failure(ShareFailure.unavailable))
        }
      }
      var supplied: NSSecureCoding?
      do {
        supplied = try box.wait(cancellation: cancellation)
        if let supplied, !(supplied is Data) { return supplied }
      } catch {
        try cancellation.check()
        guard provider.canLoadObject(ofClass: NSURL.self) else { throw error }
      }
      // Some providers return an NSURL archive as Data. Only Foundation's
      // public typed reader may decode it; never inspect that archive ourselves.
      if provider.canLoadObject(ofClass: NSURL.self) {
        let typed = ResultBox<NSSecureCoding>()
        let progress = provider.loadObject(ofClass: NSURL.self) { value, _ in
          if let url = value as? NSURL {
            typed.finish(.success(url))
          } else {
            typed.finish(.failure(ShareFailure.invalid))
          }
        }
        do { return try typed.wait(cancellation: cancellation) } catch {
          progress.cancel()
          try cancellation.check()
          if supplied == nil { throw error }
        }
      }
      guard let supplied else { throw ShareFailure.unavailable }
      return supplied
    }

    static func bytes(
      _ provider: NSItemProvider, type: String, limit: Int, timeout: TimeInterval = 10,
      cancellation: ShareCancellation? = nil
    ) throws -> Data {
      let box = ResultBox<Data>()
      let progress = provider.loadFileRepresentation(forTypeIdentifier: type) { url, _ in
        do {
          guard let url, url.isFileURL else { throw ShareFailure.unavailable }
          let descriptor = open(url.path, O_RDONLY | O_NOFOLLOW | O_NONBLOCK)
          guard descriptor >= 0 else { throw ShareFailure.unavailable }
          let handle = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
          defer { try? handle.close() }
          var info = stat()
          guard fstat(descriptor, &info) == 0, (info.st_mode & S_IFMT) == S_IFREG else {
            throw ShareFailure.invalid
          }
          guard info.st_size >= 0, info.st_size <= Int64(limit) else { throw ShareFailure.quota }
          let data = try handle.read(upToCount: limit + 1) ?? Data()
          guard data.count <= limit else { throw ShareFailure.quota }
          box.finish(.success(data))
        } catch { box.finish(.failure(error)) }
      }
      do { return try box.wait(timeout: timeout, cancellation: cancellation) } catch {
        progress.cancel()
        throw error
      }
    }

    private static func photo(
      _ provider: NSItemProvider, type: String, manifest: ShareManifest,
      store: ShareStore, deadline: Date, cancellation: ShareCancellation
    ) throws -> ShareManifest {
      let box = ResultBox<ShareManifest>()
      let stopped = ShareCancellation()
      let progress = provider.loadFileRepresentation(forTypeIdentifier: type) { url, _ in
        do {
          guard let url, url.isFileURL else { throw ShareFailure.unavailable }
          // Apple's temporary file expires when this callback returns. Stream
          // directly into protected staging while provider access is valid.
          let imported = try store.importFile(
            manifest.id, owner: manifest.owner!, source: url,
            name: provider.suggestedName ?? url.lastPathComponent, mime: "image/*", photo: true,
            check: {
              try cancellation.check()
              try stopped.check()
              guard Date() < deadline else { throw ShareFailure.expired }
            })
          box.finish(.success(imported))
        } catch { box.finish(.failure(error)) }
      }
      do {
        return try box.wait(
          timeout: min(60, max(0, deadline.timeIntervalSinceNow)), cancellation: cancellation)
      } catch {
        stopped.cancel()
        progress.cancel()
        // Pair timeout with the store lock: an already-publishing completion
        // finishes before this fence; later completions fail check under it.
        _ = try? store.refreshOwned(manifest.id, owner: manifest.owner!)
        throw error
      }
    }

    private static func text(_ provider: NSItemProvider, cancellation: ShareCancellation) throws
      -> String
    {
      // Ask for the concrete public representation when available. Some
      // cross-process providers cannot vend a file for its abstract parent.
      let type =
        provider.registeredTypeIdentifiers.first {
          UTType($0)?.conforms(to: .utf8PlainText) == true
        } ?? provider.registeredTypeIdentifiers.first {
          UTType($0)?.conforms(to: .plainText) == true
        } ?? UTType.plainText.identifier
      do {
        let data = try bytes(
          provider, type: type, limit: ShareLimits.textBytes, cancellation: cancellation)
        let encoding: String.Encoding =
          UTType(type)?.conforms(to: .utf16PlainText) == true ? .utf16 : .utf8
        guard let value = String(data: data, encoding: encoding) else {
          throw ShareFailure.invalid
        }
        guard value.utf8.count <= ShareLimits.textBytes else { throw ShareFailure.quota }
        return value
      } catch {
        // A public string provider need not supply a temporary file. Do not
        // bypass a measured size limit or retry a stalled provider this way.
        guard let failure = error as? ShareFailure, [.unavailable, .invalid].contains(failure),
          provider.canLoadObject(ofClass: NSString.self)
        else { throw error }
        log.info("share_text_file_fallback category=\(failure.rawValue, privacy: .public)")
      }
      let box = ResultBox<String>()
      let progress = provider.loadObject(ofClass: NSString.self) { value, _ in
        guard let string = value as? NSString else {
          box.finish(.failure(ShareFailure.unavailable))
          return
        }
        box.finish(Result { try boundedString(string) })
      }
      do {
        let value = try box.wait(cancellation: cancellation)
        log.info("share_text_imported representation=string")
        return value
      } catch {
        progress.cancel()
        guard (error as? ShareFailure) == .unavailable else { throw error }
      }
      // Older item providers implement loadItem but cannot supply the newer
      // NSItemProviderReading representation. Let Foundation vend the public
      // object; never decode its private archived data ourselves.
      let legacy = ResultBox<String>()
      provider.loadItem(forTypeIdentifier: type, options: nil) { value, _ in
        log.info(
          "share_text_item string=\(value is NSString) attributed=\(value is NSAttributedString) bytes=\(value is Data) url=\(value is URL)"
        )
        if let string = value as? NSString {
          legacy.finish(Result { try boundedString(string) })
        } else if let string = value as? NSAttributedString {
          guard string.length <= ShareLimits.textBytes else {
            legacy.finish(.failure(ShareFailure.quota))
            return
          }
          legacy.finish(Result { try boundedString(string.string as NSString) })
        } else if let url = value as? URL, TextShare.validURL(url.absoluteString) {
          // The public coercing interface can return the original NSURL even
          // when the source registered it under plain text. Only web URLs are
          // text here; never open a returned file URL or interpret an archive.
          legacy.finish(.success(url.absoluteString))
        } else {
          legacy.finish(.failure(ShareFailure.unavailable))
        }
      }
      let value = try legacy.wait(cancellation: cancellation)
      log.info("share_text_imported representation=item")
      return value
    }

    private static func boundedString(_ string: NSString) throws -> String {
      // The framework delivers the object; bound it before bridging/copying
      // it into our text buffers, just as for attributedContentText.
      guard string.length <= ShareLimits.textBytes else { throw ShareFailure.quota }
      let value = string as String
      guard value.utf8.count <= ShareLimits.textBytes else { throw ShareFailure.quota }
      return value
    }

    public static func importItems(
      _ items: [NSExtensionItem], store: ShareStore,
      cancellation: ShareCancellation = ShareCancellation(),
      staged: (ShareManifest) -> Void = { _ in }
    ) throws
      -> ShareManifest
    {
      try cancellation.check()
      guard items.count <= 16, items.reduce(0, { $0 + ($1.attachments?.count ?? 0) }) <= 32 else {
        throw ShareFailure.quota
      }
      var text: [String] = []
      var urls: [String] = []
      var metadata: [SourceMetadata] = []
      var failures: [String] = []
      var photos: [(NSItemProvider, String)] = []
      let deadline = Date().addingTimeInterval(120)
      for input in items {
        try cancellation.check()
        let urlStart = urls.count
        log.info(
          "share_item public_title=\(input.attributedTitle != nil) public_text=\(input.attributedContentText != nil)"
        )
        if let content = input.attributedContentText {
          // Check the attributed length before .string materializes its value.
          guard content.length <= ShareLimits.textBytes else { throw ShareFailure.quota }
          text.append(try boundedString(content.string as NSString))
        }
        for provider in input.attachments ?? [] {
          try cancellation.check()
          guard Date() < deadline else {
            failures.append("provider_timeout")
            break
          }
          // Counts and fixed known-type flags are sufficient for device input
          // measurements. Never log sender-controlled identifiers or content.
          log.info(
            "share_provider url=\(provider.hasItemConformingToTypeIdentifier(UTType.url.identifier)) text=\(provider.hasItemConformingToTypeIdentifier(UTType.plainText.identifier)) image=\(provider.hasItemConformingToTypeIdentifier(UTType.image.identifier)) representations=\(provider.registeredTypeIdentifiers.count)"
          )
          do {
            // Local image providers remain photos. Non-image providers may
            // advertise file-url as an alternative to a valid remote URL.
            let localImage = provider.hasItemConformingToTypeIdentifier(UTType.image.identifier)
              && provider.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier)
            if provider.hasItemConformingToTypeIdentifier(UTType.url.identifier) && !localImage
            {
              let urlStart = urls.count
              do {
                let value = try item(
                  provider, type: UTType.url.identifier, cancellation: cancellation)
                log.info(
                  "share_url_representation url=\(value is URL) nsurl=\(value is NSURL) text=\(value is String) bytes=\(value is Data) metadata=\(value is LPLinkMetadata)"
                )
                var raw: String?
                var supplied: SourceMetadata?
                if let link = value as? LPLinkMetadata {
                  if let url = link.originalURL ?? link.url {
                    raw = url.absoluteString
                    supplied = SourceMetadata(
                      url: url.absoluteString, title: link.title, imageProvider: link.imageProvider)
                  }
                } else if let url = value as? URL {
                  raw = url.absoluteString
                } else if let string = value as? String {
                  raw = string
                } else if let data = value as? Data, data.count <= 16 * 1024,
                  let decoded = String(data: data, encoding: .utf8)
                {
                  raw = decoded.trimmingCharacters(in: .whitespacesAndNewlines)
                } else {
                  throw ShareFailure.invalid
                }
                if let raw {
                  if TextShare.validURL(raw) {
                    urls.append(raw)
                    if let supplied { metadata.append(supplied) }
                  } else {
                    failures.append("unsupported_url")
                  }
                } else {
                  failures.append("unsupported_url")
                }
              } catch {
                failures.append((error as? ShareFailure)?.rawValue ?? "provider_unavailable")
              }
              // An image alternative on the SAME provider belongs to this
              // link. Never infer that an unrelated image attachment is its
              // thumbnail, and never send one as a standalone photo here.
              if urls.count > urlStart, let raw = urls.last,
                [UTType.png, .jpeg, .webP].contains(where: {
                  provider.hasItemConformingToTypeIdentifier($0.identifier)
                })
              {
                if let index = metadata.firstIndex(where: { $0.url == raw }) {
                  metadata[index].imageProvider = provider
                } else {
                  metadata.append(SourceMetadata(url: raw, imageProvider: provider))
                }
              }
              // Some providers expose useful accompanying prose separately.
              if provider.hasItemConformingToTypeIdentifier(UTType.plainText.identifier) {
                text.append(try Self.text(provider, cancellation: cancellation))
              }
            } else if provider.hasItemConformingToTypeIdentifier(UTType.image.identifier) {
              // Provider order is logical attachment order. Request only one
              // concrete representation, preserving the source's preference.
              let type =
                provider.registeredTypeIdentifiers.first {
                  UTType($0)?.conforms(to: .image) == true
                } ?? UTType.image.identifier
              if photos.count < ShareLimits.files {
                photos.append((provider, type))
              } else {
                failures.append("too_many_photos")
              }
            } else if provider.hasItemConformingToTypeIdentifier(UTType.plainText.identifier) {
              text.append(try Self.text(provider, cancellation: cancellation))
            } else {
              failures.append("unsupported_input")
            }
          } catch {
            let category = (error as? ShareFailure)?.rawValue ?? "provider_unavailable"
            log.error("share_provider_failed category=\(category, privacy: .public)")
            failures.append(category)
          }
        }
        let itemURLs = Array(Set(urls.dropFirst(urlStart)))
        if itemURLs.count == 1, let raw = itemURLs.first, TextShare.validURL(raw),
          let title = input.attributedTitle?.string, !title.isEmpty, title != raw
        {
          if let index = metadata.firstIndex(where: { $0.url == raw }) {
            if metadata[index].title == nil { metadata[index].title = title }
          } else {
            metadata.append(SourceMetadata(url: raw, title: title))
          }
        }
      }
      try cancellation.check()
      let normalized = try TextShare.normalize(text: text, urls: urls, allowEmpty: !photos.isEmpty)
      var manifest = try store.claim(
        store.create(text: normalized, importFailures: ["incomplete_import"]).id)
      staged(manifest)
      // A killed import must never look like an intentionally smaller batch.
      manifest.importFailures = failures + ["incomplete_import"]
      manifest = try store.update(manifest)
      staged(manifest)
      // Never silently send a subset of the user's selection. An over-limit
      // selection stages no photos; any earlier failure also stops photo and
      // preview loading. Retained partial drafts remain explicitly unsendable.
      for (provider, type) in photos {
        guard failures.isEmpty else { break }
        try cancellation.check()
        do {
          manifest = try photo(
            provider, type: type, manifest: manifest, store: store,
            deadline: deadline, cancellation: cancellation)
        } catch {
          try cancellation.check()
          let category = (error as? ShareFailure)?.rawValue ?? "provider_unavailable"
          failures.append(category)
          log.error("share_photo_failed category=\(category, privacy: .public)")
          manifest = try store.refreshOwned(manifest.id, owner: manifest.owner!)
        }
        staged(manifest)
      }
      manifest.importFailures = failures
      // Keep the incomplete marker until preview intake finishes as well.
      manifest.importFailures.append("incomplete_import")
      manifest = try store.update(manifest)
      for link in metadata {
        guard failures.isEmpty else { break }
        try cancellation.check()
        guard let raw = TextShare.matchedLink(link.url, in: normalized) else { continue }
        if (manifest.previews?.count ?? 0) >= 8
          && manifest.previews?.contains(where: { $0.matched_url == raw }) != true
        {
          manifest.importFailures.append("too_many_source_previews")
          manifest = try store.update(manifest)
          break
        }
        let title = TextShare.previewField(link.title, limit: 300)
        guard title != nil || link.imageProvider != nil else { continue }
        let preview = SharePreview(matched_url: raw, title: title)
        log.info(
          "share_source_metadata title=\(link.title != nil) image=\(link.imageProvider != nil)")
        // Only supplied PNG/JPEG/WebP bytes; no external image URLs or UIImage
        // allocation before dimension/byte validation in the shared store.
        var imported = false
        if let image = link.imageProvider {
          for type in [UTType.png, .jpeg, .webP]
          where image.hasItemConformingToTypeIdentifier(type.identifier) {
            guard Date() < deadline else { break }
            if let data = try? bytes(
              image, type: type.identifier, limit: ShareLimits.thumbnailBytes,
              cancellation: cancellation),
              let next = try? store.importThumbnail(
                manifest.id, owner: manifest.owner!, data: data, mime: type.preferredMIMEType!,
                preview: preview)
            {
              manifest = next
              imported = true
              break
            }
          }
        }
        try cancellation.check()
        if !imported {
          manifest.previews = (manifest.previews ?? []).filter { $0.matched_url != raw } + [preview]
          manifest = try store.update(manifest)
        }
      }
      try cancellation.check()
      manifest.importFailures.removeAll { $0 == "incomplete_import" }
      manifest = try store.update(manifest)
      staged(manifest)
      log.info("share_imported photos=\(photos.count) failures=\(manifest.importFailures.count)")
      return manifest
    }
  }
#endif
