import Foundation
import Security
import XCTest

@testable import AxonIncomingShare

final class PreviewTrustTests: XCTestCase {
  // Public synthetic certificates only. Keys were destroyed after generation;
  // these anchors are set on individual test SecTrust objects, never installed.
  private let root =
    "MIIDAjCCAeqgAwIBAgIUFPZCjJQduS4rjaAsYInPDjBJ20gwDQYJKoZIhvcNAQELBQAwITEfMB0GA1UEAwwWQXhvbiBQcmV2aWV3IFRlc3QgUm9vdDAeFw0yNjExMDMwMDA2NDBaFw0zNjEwMzEwMDA2NDBaMCExHzAdBgNVBAMMFkF4b24gUHJldmlldyBUZXN0IFJvb3QwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQCXndETZDbz/NxphndWmLHcOO/OepWFRwdV/5ioLPIpli6l6zjbX1CzO3cGEmnfa4OrNRJbJAbbXhCrrEWg6QqexeSSycuy+b3bSI+xAbxT/2bcJYlKSf0a9rRxQ+tERXeTYNCPmAkcject68tnYy9b1bL/UoI1hXDwPV1SqZQpEEggY7SucRWrC5X+en2DG2Ia3HW7ys/aADLD+iIG9ONutnFnaKcD1hji1/77jnpu5pDI7R4H+kj0DmC07/3e3RlmsnSp9AiJ+oDKGiQmhaMoy8auOU+cuws8LP44NcbSEn/otAeTuA3imVeSpn9eG+6eivh4q6sQZ9jSmcnPeMw1AgMBAAGjMjAwMB0GA1UdDgQWBBQKEJpYdziAYz/Xm0kSAZDPHHKG4jAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQCEw5kicmqcucit2B6WJnPn+Sfz7iID0/RWb8L+Xi/2Vsw1vxFLELuxAgzY3C4EUIMmKQQCsYi1cmcfEfePgbNSa/UapoEcLbVxCCCp6GSEGd5qmSUOuNpsyfhbsU3Jkd76JPDmcBU6R21JnFaMUtVnKyljnzEqDItHzk6VuQY6Z7b6q7Iem11+msHk2lddeUpVUHZvALMdmyjSgtMkv5G0hXM3yLca3hhACCgM5lvAaHVzEhGg5tLiYxXflgpO2G+1puKsVKubloYUJsOTWT41/k5haR4YZjjt22Sxxky07V3Ym4pokybTa7mdgytdLTJ3Agy6XQZyqWPtzhciwzsg"
  private let leaf =
    "MIIDIjCCAgqgAwIBAgIUHq9dtZoJdjohI9NFNO3fbuKErs0wDQYJKoZIhvcNAQELBQAwITEfMB0GA1UEAwwWQXhvbiBQcmV2aWV3IFRlc3QgUm9vdDAeFw0yNjExMDMwMDA2NDBaFw0yNzExMDMwMDA2NDBaMB4xHDAaBgNVBAMME3ByZXZpZXcuZXhhbXBsZS5vcmcwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQDMgwjZmTL658v+0X+nDfqaFFcv2FfAdAKsUxufGp6qUK55brOI5KANMUhZTmnQgIzM0YnhGqNhIiDqc+zmgr9xWJ6H98rGKU5k+Asw3KRbWMwG+5RxzFSWQfpUoIDg4fOo590aIerVzQ0cMRV8/8gBCX+MPKVgg0wMxH43+ztUU8naN/dpeFKVa5y03EGnMtlQuqY/1KV/tsoEDxELxfnG1MiuJsaf6od6+K8Eg3/XkgXRskYci7RSeTKy0YPJlVLlOHQ7F+KE+M1U1anUG3PSaKzWxC+m197RxFqf6BAPbIdKCFxyctIMUrufVFUJol2RxFgWkj3RhgZZeBCgzAYdAgMBAAGjVTBTMAwGA1UdEwEB/wQCMAAwDgYDVR0PAQH/BAQDAgWgMBMGA1UdJQQMMAoGCCsGAQUFBwMBMB4GA1UdEQQXMBWCE3ByZXZpZXcuZXhhbXBsZS5vcmcwDQYJKoZIhvcNAQELBQADggEBABkzZ7s6JMvFqzUumuQsDP/HBCDrSE8Wes244+X/7l2h/06paaDImM8ZzZLyxqFFNrgtZe+TH4V7X56U2IjaI6+mrLIPByJC4zkO5puW3cu4TRYsAplSWmMUqC9qlv0DPz2VUt+6TSe5SiitmQXBkkrDPQws4KcbFqgxPkTNQsl7kcnnYTQblB4jUhx2bEh4/TTSluSaYgdSmn0lnh7vSnYIH48S2QtEf7w89gBw6BaNAMHN8922MLrePpQWGhdth03mUJ48ryKvrPrGxEcK/E7ZDtW8iSigVR9PPysBKlNVPApeJ5J1IT+Kht5X/4qlL0WYZf8myLR2NcSdtvG9MMQ="
  func testCertificateHostnameChainAndExpiryAreVerified() throws {
    let root = try XCTUnwrap(
      SecCertificateCreateWithData(nil, Data(base64Encoded: root)! as CFData))
    let leaf = try XCTUnwrap(
      SecCertificateCreateWithData(nil, Data(base64Encoded: leaf)! as CFData))
    func trust() throws -> SecTrust {
      var value: SecTrust?
      XCTAssertEqual(
        SecTrustCreateWithCertificates(
          [leaf, root] as CFArray, SecPolicyCreateSSL(true, "preview.example.org" as CFString),
          &value), errSecSuccess)
      let result = try XCTUnwrap(value)
      XCTAssertEqual(SecTrustSetAnchorCertificates(result, [root] as CFArray), errSecSuccess)
      XCTAssertEqual(SecTrustSetAnchorCertificatesOnly(result, true), errSecSuccess)
      XCTAssertEqual(
        SecTrustSetVerifyDate(result, Date(timeIntervalSince1970: 1_793_668_020) as CFDate),
        errSecSuccess)
      return result
    }
    let valid = try trust()
    XCTAssertTrue(PreviewNetwork.verify(valid, host: "preview.example.org"))
    XCTAssertFalse(PreviewNetwork.verify(try trust(), host: "different.example.org"))
    let expired = try trust()
    XCTAssertEqual(
      SecTrustSetVerifyDate(expired, Date(timeIntervalSince1970: 1_856_736_420) as CFDate),
      errSecSuccess)
    XCTAssertFalse(PreviewNetwork.verify(expired, host: "preview.example.org"))
    var untrusted: SecTrust?
    XCTAssertEqual(
      SecTrustCreateWithCertificates(
        [leaf, root] as CFArray, SecPolicyCreateSSL(true, nil), &untrusted), errSecSuccess)
    XCTAssertFalse(PreviewNetwork.verify(try XCTUnwrap(untrusted), host: "preview.example.org"))
  }
}
