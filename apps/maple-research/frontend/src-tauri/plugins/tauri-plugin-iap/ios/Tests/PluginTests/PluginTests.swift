import Foundation
import XCTest
@testable import tauri_plugin_iap

final class PluginContractTests: XCTestCase {
    func testIdentifiersAreLosslessAndCanonical() throws {
        XCTAssertEqual(try parseTransactionId("18446744073709551615"), UInt64.max)
        XCTAssertEqual(try parseTransactionId("0"), 0)
        for value in ["01", "+1", "-1", "1.0", "1e2", " 1", "", "18446744073709551616"] {
            XCTAssertThrowsError(try parseTransactionId(value))
        }
        let payload = SignedTransactionPayload(transactionId: "18446744073709551615", originalTransactionId: "9007199254740993", productId: "pro", jws: "fixture")
        let encoded = try JSONEncoder().encode(payload)
        XCTAssertEqual(try JSONDecoder().decode(SignedTransactionPayload.self, from: encoded), payload)
    }

    func testRecoveryDeduplicatesAndKeepsNewestSignedVersionWithoutLosingUnfinishedState() {
        var collection = SignedTransactionCollection()
        let old = SignedTransactionPayload(transactionId: "123", originalTransactionId: "1", productId: "pro", jws: "old-fixture")
        let new = SignedTransactionPayload(transactionId: "123", originalTransactionId: "1", productId: "pro", jws: "new-fixture")
        collection.insert(new, signedDate: Date(timeIntervalSince1970: 2), isUnfinished: false)
        collection.insert(old, signedDate: Date(timeIntervalSince1970: 1), isUnfinished: true)
        XCTAssertEqual(collection.payload.transactions, [new])
        XCTAssertEqual(collection.payload.unfinishedTransactionIds, ["123"])
    }

    func testPendingAndCancellationNeverFabricateTransaction() throws {
        for (outcome, status) in [(PurchaseOutcome.pending, "pending"), (.cancelled, "cancelled")] {
            let object = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(outcome)) as? [String: String])
            XCTAssertEqual(object, ["status": status])
        }
    }

    func testUnknownStorefrontIsExplicitlyNull() throws {
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(StorefrontPayload(countryCode: nil))) as? [String: Any])
        XCTAssertTrue(object["countryCode"] is NSNull)
    }
}
