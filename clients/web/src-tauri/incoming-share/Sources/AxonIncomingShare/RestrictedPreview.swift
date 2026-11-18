import Foundation
import SwiftSoup
import os

/// No scripts, styles, frames, favicons or arbitrary page subresources load.
/// Only an explicit page and its selected preview image reach PreviewNetwork.
enum RestrictedPreview {
  private static let log = Logger(subsystem: "org.matrixaxon.axon", category: "device-preview")
  static let initialHeadBytes = 256 * 1024
  static let maxHeadBytes = 1024 * 1024
  static let maxTags = 512
  private static let slot = DispatchSemaphore(value: 1)
  struct Metadata {
    var originalURL: URL
    var url: URL
    var title: String?
    var description: String?
    var siteName: String?
    var image: Data?
    var imageUnavailable = false
  }
  typealias Fetch = (URL, PreviewNetwork.Kind, DispatchTime, ShareCancellation) throws ->
    PreviewNetwork.Response

  static func fetch(
    _ url: URL, cancellation: ShareCancellation,
    request: Fetch = { try PreviewNetwork.fetch($0, kind: $1, deadline: $2, cancellation: $3) }
  ) throws -> Metadata {
    try cancellation.check()
    guard slot.wait(timeout: .now() + 1) == .success else { throw ShareFailure.previewBusy }
    defer { slot.signal() }
    let deadline = DispatchTime.now() + 25
    let response = try request(url, .page, deadline, cancellation)
    try cancellation.check()
    if case .image = response.media {
      return Metadata(
        originalURL: url, url: response.url,
        title: TextShare.previewField(
          response.url.lastPathComponent.removingPercentEncoding, limit: 300), image: response.body)
    }
    guard case .html(let type) = response.media else { throw ShareFailure.previewUnavailable }
    let page = try parse(response.body, contentType: type, cutoff: response.cutoff)
    var metadata = Metadata(
      originalURL: url, url: response.url, title: page.title,
      description: page.description, siteName: page.siteName)
    if let image = page.image {
      // A named image that cannot be used counts as unavailable, whether it
      // fails to resolve here or to load below.
      metadata.imageUnavailable = true
      if let target = URL(string: image, relativeTo: response.url)?.absoluteURL {
        do {
          let response = try request(target, .image, deadline, cancellation)
          guard response.body.count <= ShareLimits.thumbnailBytes else { throw ShareFailure.quota }
          metadata.image = response.body
          metadata.imageUnavailable = false
        } catch {
          try cancellation.check()
          log.info("device_preview_image_unavailable")
        }
      } else {
        log.info("device_preview_image_unavailable")
      }
    }
    guard
      metadata.title != nil || metadata.description != nil || metadata.siteName != nil
        || metadata.image != nil
    else { throw ShareFailure.noPreviewMetadata }
    log.info("device_preview_fetched retained_bytes=\(metadata.image?.count ?? 0)")
    return metadata
  }

  struct Page {
    var title: String?
    var description: String?
    var siteName: String?
    var image: String?
  }
  static func parse(_ data: Data, contentType: String = "text/html", cutoff: Bool = false)
    throws -> Page
  {
    guard data.count <= maxHeadBytes else { throw ShareFailure.quota }
    // Bound the parser's potential tree and recursive destruction before DOM
    // allocation: only the bytes before the 513th `<` are parsed. Preview
    // tags come early in a head, so a heavy page keeps its metadata rather
    // than its card. A tag the cut ends is dropped whole. The Rust reader
    // (`axon-link-preview/src/page.rs`) makes the same cut.
    let bounded = boundedHead(data)
    let html: String
    if let utf8 = String(data: bounded, encoding: .utf8) {
      html = utf8
    } else if contentType.contains("iso-8859-1") || contentType.contains("windows-1252"),
      let legacy = String(data: bounded, encoding: .windowsCP1252)
    {
      html = legacy
    } else {
      html = String(decoding: bounded, as: UTF8.self)
    }
    let document = try SwiftSoup.parse(html)
    func content(_ selectors: [String], limit: Int) throws -> String? {
      for selector in selectors {
        if let node = try document.select(selector).first(),
          let value = TextShare.previewField(try node.attr("content"), limit: limit)
        {
          return value
        }
      }
      return nil
    }
    func documentTitle() throws -> String? {
      guard let title = TextShare.previewField(try document.title(), limit: 300) else { return nil }
      if (cutoff || bounded.count < data.count) && !headProbe(bounded, collectMeta: false).completeTitle {
        return nil
      }
      return title
    }
    let title =
      try content(["meta[property=og:title]", "meta[name=twitter:title]"], limit: 300)
      ?? documentTitle()
    return try Page(
      title: title,
      description: content(
        [
          "meta[property=og:description]", "meta[name=twitter:description]",
          "meta[name=description]",
        ], limit: 1000),
      siteName: content(["meta[property=og:site_name]"], limit: 120),
      image: content(
        [
          "meta[property=og:image:secure_url]", "meta[property=og:image]",
          "meta[name=twitter:image]", "meta[name=twitter:image:src]",
        ], limit: 4096))
  }

  private static func boundedHead(_ data: Data) -> Data {
    var tags = 0
    return data.prefix { byte in
      if byte == 60 { tags += 1 }
      return tags <= maxTags
    }
  }

  // Probe only complete meta tags. SwiftSoup builds the full document once,
  // when the transport has finished collecting the head.
  static func hasRichMetadata(_ data: Data, contentType: String) throws -> Bool {
    let probe = headProbe(data, collectMeta: true)
    guard !probe.openTitle, !probe.meta.isEmpty else { return false }
    let page = try parse(probe.meta, contentType: contentType)
    return page.description != nil || page.image != nil
  }

  private struct HeadProbe {
    var meta = Data()
    var completeTitle = false
    var openTitle = false
  }

  // As in the Rust reader, uncertain markup can defer the early stop.
  // Raw text and comments must never supply metadata hints.
  private static func headProbe(_ data: Data, collectMeta: Bool) -> HeadProbe {
    let bytes = Array(data)
    let rawText = ["script", "style", "noframes", "noscript"]
    let headTags = ["html", "head", "link", "base", "basefont", "bgsound", "!doctype"]
    let comment = Array("<!--".utf8)
    let commentEnd = Array("-->".utf8)
    var probe = HeadProbe()
    var raw: String?
    var cursor = 0
    while let start = bytes[cursor...].firstIndex(of: 60) {
      cursor = start + 1
      if let name = raw {
        if name == "script" && bytes[start...].starts(with: comment) { break }
        let nameEnd = start + 2 + name.utf8.count
        guard nameEnd < bytes.count, bytes[start + 1] == 47, tagBoundary(bytes[nameEnd]),
          String(decoding: bytes[(start + 2)..<nameEnd], as: UTF8.self).lowercased() == name
        else { continue }
        guard let end = tagEnd(bytes, startingAt: cursor) else { break }
        if name == "title" {
          probe.completeTitle = true
          probe.openTitle = false
        }
        raw = nil
        cursor = end
        continue
      }
      if bytes[start...].starts(with: comment) {
        guard let end = data.range(of: Data(commentEnd), in: (data.startIndex + start + 4)..<data.endIndex)
        else { break }
        cursor = end.upperBound - data.startIndex
        continue
      }
      guard let end = tagEnd(bytes, startingAt: cursor),
        let nameEnd = bytes[cursor..<end].firstIndex(where: tagBoundary)
      else { break }
      let name = String(decoding: bytes[cursor..<nameEnd], as: UTF8.self).lowercased()
      if name == "meta" {
        if collectMeta {
          guard probe.meta.count + end - start <= 16 * 1024 else {
            probe.meta = Data()
            break
          }
          probe.meta.append(contentsOf: bytes[start..<end])
        }
      } else if name == "title" {
        raw = name
        probe.openTitle = true
      } else if rawText.contains(name) {
        raw = name
      } else if !headTags.contains(name) {
        break
      }
      cursor = end
    }
    return probe
  }

  private static func tagBoundary(_ byte: UInt8) -> Bool {
    switch byte {
    case 9, 10, 12, 13, 32, 47, 62: return true
    default: return false
    }
  }

  private static func tagEnd(_ bytes: [UInt8], startingAt start: Int) -> Int? {
    var quote: UInt8?
    for index in start..<bytes.count {
      let byte = bytes[index]
      if let open = quote {
        if byte == open { quote = nil }
      } else if byte == 34 || byte == 39 {
        quote = byte
      } else if byte == 62 {
        return index + 1
      }
    }
    return nil
  }

}
