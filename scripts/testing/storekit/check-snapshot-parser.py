#!/usr/bin/env python3
"""Run the exact Foundation-only capture parser on the host, never a simulator."""
import argparse
import os
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--snapshot", type=Path, help="Also validate a saved real XCTest AX snapshot")
    args = parser.parse_args()
    if not os.environ.get("DEVELOPER_DIR"):
        parser.error("Set DEVELOPER_DIR to the intended installed Xcode")
    root = Path(__file__).resolve().parents[3]
    source = root / "apps/maple-research/frontend/src-tauri/tests/storekit/MapleStoreKitUITests.swift"
    text = source.read_text()
    implementation = text.split("// BEGIN SNAPSHOT PARSER:", 1)[1].split("\n", 1)[1].split("// END SNAPSHOT PARSER", 1)[0]
    fixtures = r'''
func require(_ condition: @autoclosure () -> Bool, _ message: String) {
    guard condition() else { fatalError(message) }
}
func rejects(_ input: String, _ message: String) {
    do { _ = try storeKitStatusSnapshot(input); fatalError(message) }
    catch { }
}
let status: [String: Any] = [
    "purchaseStatus": "success", "environment": "Xcode",
    "localSigningCertificate": "AQID", "nested": ["count": 1],
    "text": "Apostrophe ' and braces { } and escaped quote \" and slash \\"
]
let data = try JSONSerialization.data(withJSONObject: status, options: [.prettyPrinted, .sortedKeys])
let json = String(decoding: data, as: UTF8.self)
let label = "StaticText, {{1.0, 2.0}, {3.0, 4.0}}, label: '\(json)', value: {\n  \"purchaseStatu..., Focused"
let captured = try storeKitStatusSnapshot(label)
require(captured["environment"] as? String == "Xcode", "Real AX label shape must parse")
require(captured["text"] as? String == status["text"] as? String, "JSON string escapes must survive")
let duplicate = try storeKitStatusSnapshot(label + "\nvalue: " + json)
require(duplicate["purchaseStatus"] as? String == "success", "Identical label/value copies must agree")
let quotedValue = try storeKitStatusSnapshot("value: '\(json)'")
require(quotedValue["environment"] as? String == "Xcode", "Quoted AX value must parse")
rejects("StaticText, label: 'No diagnostic JSON'", "Missing status must fail")
rejects("label: '{\"purchaseStatus\":\"success\"", "Truncated JSON must fail")
rejects("label: '{\"purchaseStatus\":}'", "Malformed JSON must fail")
rejects(label + "\nlabel: '{\"purchaseStatus\":\"pending\"}'", "Conflicting snapshots must fail")
rejects("label: '{\"purchaseStatus\":\"" + String(repeating: "x", count: 65_536) + "\"}'", "Oversized candidate must fail")
rejects(String(repeating: "x", count: 2_000_001), "Oversized snapshot must fail")
let purchasedCertificate = try storeKitCertificateFromSnapshot(status)
require(purchasedCertificate == Data([1, 2, 3]), "Successful purchase certificate must export")
var recovery = status
recovery["purchaseStatus"] = "idle"
recovery["certificateSource"] = "verified_recovery"
recovery["signedTransactionsPresent"] = true
recovery["transactionCount"] = 1
let recoveredCertificate = try storeKitCertificateFromSnapshot(recovery)
require(recoveredCertificate == Data([1, 2, 3]), "Native-verified recovery certificate must export")
require(recovery["purchaseStatus"] as? String == "idle", "Recovery must not imply a successful purchase")
func rejectsCertificate(_ object: [String: Any], _ message: String) {
    do { _ = try storeKitCertificateFromSnapshot(object); fatalError(message) }
    catch { }
}
var invalid = recovery
invalid["signedTransactionsPresent"] = false
rejectsCertificate(invalid, "Unverified recovery must fail")
invalid = recovery
invalid["transactionCount"] = 0
rejectsCertificate(invalid, "Empty listing must fail")
invalid = recovery
invalid["certificateSource"] = "verified_purchase"
rejectsCertificate(invalid, "Recovery source must be explicit")
invalid = recovery
invalid["environment"] = "Sandbox"
rejectsCertificate(invalid, "Non-Xcode certificate must fail")
invalid = recovery
invalid["localSigningCertificate"] = ""
rejectsCertificate(invalid, "Empty certificate must fail")
if CommandLine.arguments.count == 2 {
    let saved = try String(contentsOfFile: CommandLine.arguments[1], encoding: .utf8)
    let actual = try storeKitStatusSnapshot(saved)
    require(actual["environment"] as? String == "Xcode", "Saved snapshot must be Xcode environment")
    let certificate = try storeKitCertificateFromSnapshot(actual)
    require(!certificate.isEmpty, "Saved snapshot must contain its public certificate")
    print("Saved real AX snapshot parsed; public certificate present")
}
print("18 snapshot parser and certificate-source fixture checks passed")
'''
    with tempfile.TemporaryDirectory(prefix="maple-storekit-parser-") as directory:
        fixture = Path(directory) / "SnapshotParserCheck.swift"
        fixture.write_text("import Foundation\n" + implementation + fixtures)
        command = ["/usr/bin/xcrun", "swift", str(fixture)]
        if args.snapshot:
            command.append(str(args.snapshot.resolve(strict=True)))
        result = subprocess.run(command, timeout=45, check=False)
        return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
