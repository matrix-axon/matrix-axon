import Foundation

public enum PreviewContract {
  public static func validateWire(_ value: Any, schema: String) -> Bool {
    guard let definition = WireSchemas.schemas[schema] else { return false }
    return validate(value, definition)
  }

  private static func validate(_ value: Any, _ schema: [String: Any]) -> Bool {
    if let ref = schema["$ref"] as? String {
      return validateWire(value, schema: String(ref.split(separator: "/").last!))
    }
    if let alternatives = schema["oneOf"] as? [[String: Any]] {
      return alternatives.filter { validate(value, $0) }.count == 1
    }
    if let options = schema["enum"] as? [String], let string = value as? String,
      !options.contains(string)
    {
      return false
    }
    let types = schema["type"] as? [String] ?? (schema["type"] as? String).map { [$0] } ?? []
    if value is NSNull { return types.contains("null") }
    if types.contains("object") {
      guard let object = value as? [String: Any] else { return false }
      let properties = schema["properties"] as? [String: [String: Any]] ?? [:]
      if let required = schema["required"] as? [String], !required.allSatisfy({ object[$0] != nil })
      {
        return false
      }
      for (key, entry) in object {
        if let definition = properties[key] {
          if !validate(entry, definition) { return false }
        } else if schema["additionalProperties"] as? Bool == false {
          return false
        }
      }
      return true
    }
    if types.contains("array") {
      guard let array = value as? [Any] else { return false }
      if let max = schema["maxItems"] as? Int, array.count > max { return false }
      return array.allSatisfy { validate($0, schema["items"] as? [String: Any] ?? [:]) }
    }
    if types.contains("string") {
      guard let string = value as? String else { return false }
      return schema["format"] as? String != "uuid" || UUID(uuidString: string) != nil
    }
    return types.isEmpty
  }

  /// Same fixture entry point as the production server DTO/preflight test.
  public static func check(_ request: [String: Any], media: Bool = false) -> String? {
    guard validateWire(request, schema: media ? "SendMediaRequest" : "SendMessageRequest") else {
      return "invalid"
    }
    guard let previews = request["url_previews"] as? [[String: Any]] else { return nil }
    let body = request[media ? "caption" : "body"] as? String ?? ""
    guard body.utf8.count <= 64 * 1024 else { return "event_too_large" }
    guard previews.count <= 8 else { return "invalid" }
    let expression = try! NSRegularExpression(pattern: "(?:https?://|matrix:)[^\\s\u{feff}<>\"]+")
    let nsBody = body as NSString
    let urls = Set(
      expression.matches(in: body, range: NSRange(location: 0, length: nsBody.length)).map {
        nsBody.substring(with: $0.range).trimmingCharacters(
          in: CharacterSet(charactersIn: ".,;:!?)]'\""))
      }.filter { !$0.hasPrefix("matrix:") })
    var seen = Set<String>()
    var entries: [[String: Any]] = []
    for preview in previews {
      guard let raw = preview["matched_url"] as? String, raw.utf16.count <= 4096,
        let url = URLComponents(string: raw), ["http", "https"].contains(url.scheme),
        url.host != nil, url.user == nil, url.password == nil, urls.contains(raw),
        seen.insert(raw).inserted
      else { return "invalid" }
      var entry: [String: Any] = ["matrix:matched_url": raw]
      for (key, limit) in [("title", 300), ("description", 1000), ("site_name", 120)] {
        if let string = preview[key] as? String {
          guard string.utf16.count <= limit else { return "invalid" }
          entry["og:" + key] = string
        }
      }
      if preview["image_upload_id"] is String {
        entry["matrix:image:encrypted"] = String(repeating: "x", count: 1280)
      }
      entries.append(entry)
    }
    guard size(entries) <= 32 * 1024 else { return "preview_too_large" }
    var content: [String: Any] = ["msgtype": "m.text", "body": body, "m.url_previews": entries]
    let format = request["format"] as? String
    let formatted = request["formatted_body"] as? String
    guard (format == nil) == (formatted == nil), format == nil || format == "org.matrix.custom.html"
    else { return "invalid" }
    if let formatted {
      content["format"] = format
      content["formatted_body"] = formatted
    }
    var relation: [String: Any] = [:]
    if let root = request["thread_root"] as? String {
      relation["rel_type"] = "m.thread"
      relation["event_id"] = root
    }
    if let reply = request["reply_to"] as? String {
      relation["m.in_reply_to"] = ["event_id": reply]
    }
    if !relation.isEmpty { content["m.relates_to"] = relation }
    content["com.beeper.linkpreviews"] = entries.map { entry -> [String: Any] in
      var result = entry
      result["matched_url"] = result.removeValue(forKey: "matrix:matched_url")
      if let image = result.removeValue(forKey: "matrix:image:encrypted") {
        result["beeper:image:encryption"] = image
      }
      return result
    }
    if media { content["_reserved_media_fields"] = String(repeating: "x", count: 12 * 1024) }
    return size(content) > 40 * 1024 ? "event_too_large" : nil
  }

  private static func size(_ value: Any) -> Int {
    (try? JSONSerialization.data(
      withJSONObject: value, options: [.sortedKeys, .withoutEscapingSlashes]
    ).count) ?? Int.max
  }
}
