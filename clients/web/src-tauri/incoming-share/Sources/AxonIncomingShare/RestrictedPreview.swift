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
  private static let titleLimit = 300
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
    var html: String
    if let utf8 = String(data: bounded, encoding: .utf8) {
      html = utf8
    } else if contentType.contains("iso-8859-1") || contentType.contains("windows-1252"),
      let legacy = String(data: bounded, encoding: .windowsCP1252)
    {
      html = legacy
    } else {
      html = String(decoding: bounded, as: UTF8.self)
    }
    if cutoff || bounded.count < data.count {
      // The parser adds this text to an open title, which exceeds titleLimit.
      // A closed title keeps the original text.
      html += "\n" + String(repeating: "x", count: titleLimit + 1)
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
    let title =
      try content(["meta[property=og:title]", "meta[name=twitter:title]"], limit: titleLimit)
      ?? TextShare.previewField(try document.title(), limit: titleLimit)
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
    let probe = headProbe(data)
    guard !probe.openTitle, !probe.meta.isEmpty else { return false }
    let page = try parse(probe.meta, contentType: contentType)
    return page.description != nil || page.image != nil
  }

  private struct HeadProbe {
    var meta = Data()
    var openTitle = false
  }

  private static let rawText = [
    "title", "textarea", "script", "style", "iframe", "xmp", "noembed", "noframes", "noscript",
  ]
  private static let comment = Array("<!--".utf8)
  private static let commentEnd = Array("-->".utf8)

  // The DOM parser owns field selection. Raw text and template contents
  // cannot supply early metadata hints.
  private static func headProbe(_ data: Data) -> HeadProbe {
    let bytes = Array(data)
    var probe = HeadProbe()
    var templates = 0
    var cursor = 0
    while let start = bytes[cursor...].firstIndex(of: 60) {
      cursor = start + 1
      if bytes[start...].starts(with: comment) {
        guard let end = data.range(of: Data(commentEnd), in: (data.startIndex + start + 4)..<data.endIndex)
        else { break }
        cursor = end.upperBound - data.startIndex
        continue
      }
      let closing = cursor < bytes.count && bytes[cursor] == 47
      let nameStart = cursor + (closing ? 1 : 0)
      guard nameStart < bytes.count else { break }
      let first = bytes[nameStart]
      if !(65...90).contains(first) && !(97...122).contains(first) {
        if first == 33 || first == 63 {
          guard let end = bytes[nameStart...].firstIndex(of: 62) else { break }
          cursor = end + 1
        }
        continue
      }
      guard let nameEnd = bytes[nameStart...].firstIndex(where: tagBoundary),
        let end = tagEnd(bytes, startingAt: nameEnd)
      else { break }
      let name = String(decoding: bytes[nameStart..<nameEnd], as: UTF8.self).lowercased()
      cursor = end
      if name == "template" {
        templates = closing ? max(0, templates - 1) : templates + 1
      } else if !closing {
        if name == "plaintext" { break }
        if rawText.contains(name) {
          guard let after = skipRawText(bytes, startingAt: cursor, name: name) else {
            probe.openTitle = name == "title" && templates == 0
            break
          }
          cursor = after
        } else if name == "meta" && templates == 0 {
          guard probe.meta.count + end - start <= 16 * 1024 else {
            probe.meta = Data()
            break
          }
          probe.meta.append(contentsOf: bytes[start..<end])
        }
      }
    }
    return probe
  }

  // As in the Rust reader, the first closing script marker in a double
  // escape returns to escaped script text without closing the element.
  private static func skipRawText(_ bytes: [UInt8], startingAt start: Int, name: String) -> Int? {
    let script = name == "script"
    let nameBytes = Array(name.utf8)
    var escaped = false
    var doubleEscaped = false
    var cursor = start
    while let next = bytes[cursor...].firstIndex(where: { $0 == 60 || (escaped && $0 == 45) }) {
      cursor = next + 1
      if script && !escaped && bytes[next...].starts(with: comment) {
        escaped = true
        cursor = next + comment.count
        continue
      }
      if escaped && bytes[next...].starts(with: commentEnd) {
        escaped = false
        doubleEscaped = false
      } else if bytes[next] == 60 && escaped && !doubleEscaped
        && startsNamedTag(bytes, at: cursor, name: nameBytes)
      {
        doubleEscaped = true
      } else if bytes[next] == 60 && cursor < bytes.count && bytes[cursor] == 47
        && startsNamedTag(bytes, at: cursor + 1, name: nameBytes)
      {
        if doubleEscaped {
          doubleEscaped = false
        } else {
          return tagEnd(bytes, startingAt: cursor + 1 + nameBytes.count)
        }
      }
    }
    return nil
  }

  private static func startsNamedTag(_ bytes: [UInt8], at start: Int, name: [UInt8]) -> Bool {
    let end = start + name.count
    guard end < bytes.count, tagBoundary(bytes[end]) else { return false }
    return zip(bytes[start..<end], name).allSatisfy { byte, lower in
      ((65...90).contains(byte) ? byte + 32 : byte) == lower
    }
  }

  private static func tagBoundary(_ byte: UInt8) -> Bool {
    htmlSpace(byte) || byte == 47 || byte == 62
  }

  private static func htmlSpace(_ byte: UInt8) -> Bool {
    switch byte {
    case 9, 10, 12, 13, 32: return true
    default: return false
    }
  }

  private static func tagEnd(_ bytes: [UInt8], startingAt start: Int) -> Int? {
    enum Attribute { case name, beforeValue, unquoted, quoted(UInt8) }
    var state = Attribute.name
    for index in start..<bytes.count {
      let byte = bytes[index]
      switch (state, byte) {
      case (.quoted(let open), _) where open == byte: state = .name
      case (.quoted, _): continue
      case (_, 62): return index + 1
      case (.name, 61): state = .beforeValue
      case (.beforeValue, 34), (.beforeValue, 39): state = .quoted(byte)
      case (.beforeValue, _) where htmlSpace(byte): continue
      case (.beforeValue, _): state = .unquoted
      case (.unquoted, _) where !htmlSpace(byte): continue
      default: state = .name
      }
    }
    return nil
  }
}
