import Foundation
import StoreKit
import Tauri
import UIKit
import WebKit

struct GetProductsArgs: Decodable { let productIds: [String] }
struct PurchaseArgs: Decodable { let productId: String; let appAccountToken: String }
struct FinishArgs: Decodable { let transactionId: String }

// No CustomStringConvertible/DebugDescription: never render a signed payload in logs.
struct SignedTransactionPayload: Codable, Equatable {
    let transactionId: String
    let originalTransactionId: String
    let productId: String
    let jws: String
}

struct SignedTransactionsPayload: Encodable {
    let transactions: [SignedTransactionPayload]
    let unfinishedTransactionIds: [String]
}

struct StorefrontPayload: Encodable {
    let countryCode: String?
    enum CodingKeys: String, CodingKey { case countryCode }
    func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: CodingKeys.self)
        try values.encode(countryCode, forKey: .countryCode)
    }
}

struct ProductPayload: Encodable {
    let productId: String
    let title: String
    let description: String
    let formattedPrice: String
    let priceCurrencyCode: String
    let subscriptionPeriod: String?
    enum CodingKeys: String, CodingKey {
        case productId, title, description, formattedPrice, priceCurrencyCode, subscriptionPeriod
    }
    func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: CodingKeys.self)
        try values.encode(productId, forKey: .productId)
        try values.encode(title, forKey: .title)
        try values.encode(description, forKey: .description)
        try values.encode(formattedPrice, forKey: .formattedPrice)
        try values.encode(priceCurrencyCode, forKey: .priceCurrencyCode)
        try values.encode(subscriptionPeriod, forKey: .subscriptionPeriod)
    }
}

struct ProductsPayload: Encodable { let products: [ProductPayload] }

enum PurchaseOutcome: Encodable {
    case success(SignedTransactionPayload)
    case pending
    case cancelled
    enum CodingKeys: String, CodingKey { case status, transaction }
    func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .success(let transaction):
            try values.encode("success", forKey: .status)
            try values.encode(transaction, forKey: .transaction)
        case .pending: try values.encode("pending", forKey: .status)
        case .cancelled: try values.encode("cancelled", forKey: .status)
        }
    }
}

enum IapFailure: String, Error {
    case invalidRequest = "invalid_request"
    case invalidAccountToken = "invalid_app_account_token"
    case invalidTransactionId = "invalid_transaction_id"
    case productUnavailable = "product_unavailable"
    case unsupportedProduct = "unsupported_product_type"
    case purchaseInProgress = "purchase_in_progress"
    case verificationFailed = "transaction_verification_failed"
    case transactionNotFound = "transaction_not_found"
    case unknownPurchaseResult = "unknown_purchase_result"
    case manageUnavailable = "manage_subscriptions_unavailable"
}

// Union the sequences by transaction ID without requiring product metadata. A newer
// signed version wins, and membership in unfinished is tracked independently.
struct SignedTransactionCollection {
    private var rows: [String: (Date, SignedTransactionPayload)] = [:]
    private var unfinished = Set<String>()

    mutating func markUnfinished(_ transactionId: String) {
        unfinished.insert(transactionId)
    }

    mutating func insert(_ value: SignedTransactionPayload, signedDate: Date, isUnfinished: Bool) {
        if isUnfinished { unfinished.insert(value.transactionId) }
        if let previous = rows[value.transactionId], previous.0 > signedDate { return }
        rows[value.transactionId] = (signedDate, value)
    }

    var payload: SignedTransactionsPayload {
        SignedTransactionsPayload(
            transactions: rows.keys.sorted().compactMap { rows[$0]?.1 },
            unfinishedTransactionIds: unfinished.sorted()
        )
    }
}

func parseTransactionId(_ value: String) throws -> UInt64 {
    guard let id = UInt64(value), String(id) == value else {
        throw IapFailure.invalidTransactionId
    }
    return id
}

@available(iOS 16.0, *)
func signedPayload(_ result: VerificationResult<Transaction>) throws -> SignedTransactionPayload {
    guard case .verified(let transaction) = result else { throw IapFailure.verificationFailed }
    return SignedTransactionPayload(
        transactionId: String(transaction.id),
        originalTransactionId: String(transaction.originalID),
        productId: transaction.productID,
        jws: result.jwsRepresentation
    )
}

@available(iOS 16.0, *)
actor StoreKitClient {
    private var purchasing = false

    func purchase(productId: String, appAccountToken: String) async throws -> PurchaseOutcome {
        guard !productId.isEmpty else { throw IapFailure.invalidRequest }
        guard let token = UUID(uuidString: appAccountToken) else { throw IapFailure.invalidAccountToken }
        guard !purchasing else { throw IapFailure.purchaseInProgress }
        purchasing = true
        defer { purchasing = false }
        guard let product = try await Product.products(for: [productId]).first else {
            throw IapFailure.productUnavailable
        }
        guard product.type == .autoRenewable else { throw IapFailure.unsupportedProduct }
        switch try await product.purchase(options: [.appAccountToken(token)]) {
        case .success(let result): return .success(try signedPayload(result))
        case .pending: return .pending
        case .userCancelled: return .cancelled
        @unknown default: throw IapFailure.unknownPurchaseResult
        }
    }

    func getSignedTransactions() async -> SignedTransactionsPayload {
        var collection = SignedTransactionCollection()
        for await result in Transaction.unfinished {
            if case .verified(let transaction) = result, let value = try? signedPayload(result) {
                collection.insert(value, signedDate: transaction.signedDate, isUnfinished: true)
            } else if case .unverified(let transaction, _) = result {
                // Diagnostic queue membership is not an entitlement or a verified JWS.
                collection.markUnfinished(String(transaction.id))
            }
        }
        for await result in Transaction.currentEntitlements {
            if case .verified(let transaction) = result, let value = try? signedPayload(result) {
                collection.insert(value, signedDate: transaction.signedDate, isUnfinished: false)
            }
        }
        return collection.payload
    }

    func finishTransaction(_ value: String) async throws {
        let id = try parseTransactionId(value)
        var transaction = try await findTransaction(id, in: Transaction.unfinished)
        if transaction == nil {
            transaction = try await findTransaction(id, in: Transaction.currentEntitlements)
        }
        if transaction == nil {
            // History is only enumerated when both narrower sequences omit the ID.
            // This also finds earlier subscription terms and already-finished IDs.
            transaction = try await findTransaction(id, in: Transaction.all)
        }
        guard let transaction else { throw IapFailure.transactionNotFound }
        // The only finish site. The caller must have durably acknowledged
        // this exact transaction on its server before invoking this command.
        await transaction.finish()
    }

    private func findTransaction(_ id: UInt64, in transactions: Transaction.Transactions) async throws -> Transaction? {
        for await result in transactions {
            switch result {
            case .verified(let transaction) where transaction.id == id:
                return transaction
            case .unverified(let transaction, _) where transaction.id == id:
                throw IapFailure.verificationFailed
            default:
                continue
            }
        }
        return nil
    }
}

@available(iOS 16.0, *)
class IapPlugin: Plugin {
    private let client = StoreKitClient()
    private var transactionTask: Task<Void, Never>?
    private var storefrontTask: Task<Void, Never>?
    private weak var appWebView: WKWebView?

    override func load(webview: WKWebView) {
        super.load(webview: webview)
        appWebView = webview
        transactionTask?.cancel()
        storefrontTask?.cancel()
        transactionTask = Task { [weak self] in
            for await result in Transaction.updates {
                guard !Task.isCancelled else { break }
                guard let payload = try? signedPayload(result) else { continue }
                // No product lookup, and no finish, even if no JS listener exists.
                // StoreKit owns queue membership; callers recover through its transaction sequences.
                await MainActor.run { [weak self] in
                    guard !Task.isCancelled else { return }
                    try? self?.trigger("transactionUpdated", data: payload)
                }
            }
        }
        storefrontTask = Task { [weak self] in
            for await storefront in Storefront.updates {
                guard !Task.isCancelled else { break }
                let payload = StorefrontPayload(countryCode: storefront.countryCode)
                await MainActor.run { [weak self] in
                    guard !Task.isCancelled else { return }
                    try? self?.trigger("storefrontChanged", data: payload)
                }
            }
        }
    }

    deinit {
        transactionTask?.cancel()
        storefrontTask?.cancel()
    }

    @objc func getProducts(_ invoke: Invoke) async throws {
        let args: GetProductsArgs
        do { args = try invoke.parseArgs(GetProductsArgs.self) }
        catch { invoke.reject(IapFailure.invalidRequest.rawValue); return }
        do {
            let products = try await Product.products(for: args.productIds)
            let values = products.filter { $0.type == .autoRenewable }.map { product in
                ProductPayload(
                    productId: product.id, title: product.displayName,
                    description: product.description, formattedPrice: product.displayPrice,
                    priceCurrencyCode: product.priceFormatStyle.currencyCode,
                    subscriptionPeriod: product.subscription.map { Self.formatPeriod($0.subscriptionPeriod) }
                )
            }
            invoke.resolve(ProductsPayload(products: values))
        } catch { reject(invoke, error, fallback: "products_unavailable") }
    }

    @objc func purchase(_ invoke: Invoke) async throws {
        let args: PurchaseArgs
        do { args = try invoke.parseArgs(PurchaseArgs.self) }
        catch { invoke.reject(IapFailure.invalidRequest.rawValue); return }
        do { invoke.resolve(try await client.purchase(productId: args.productId, appAccountToken: args.appAccountToken)) }
        catch { reject(invoke, error, fallback: "purchase_failed") }
    }

    @objc func getSignedTransactions(_ invoke: Invoke) async throws {
        invoke.resolve(await client.getSignedTransactions())
    }

    @objc func finishTransaction(_ invoke: Invoke) async throws {
        let args: FinishArgs
        do { args = try invoke.parseArgs(FinishArgs.self) }
        catch { invoke.reject(IapFailure.invalidRequest.rawValue); return }
        do { try await client.finishTransaction(args.transactionId); invoke.resolve() }
        catch { reject(invoke, error, fallback: "finish_failed") }
    }

    @objc func sync(_ invoke: Invoke) async throws {
        do { try await AppStore.sync(); invoke.resolve() }
        catch { reject(invoke, error, fallback: "sync_failed") }
    }

    @objc func getStorefront(_ invoke: Invoke) async throws {
        invoke.resolve(StorefrontPayload(countryCode: await Storefront.current?.countryCode))
    }

    @MainActor @objc func manageSubscriptions(_ invoke: Invoke) async throws {
        let scene = appWebView?.window?.windowScene ?? UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .first { $0.activationState == .foregroundActive }
        guard let scene else { invoke.reject(IapFailure.manageUnavailable.rawValue); return }
        do { try await AppStore.showManageSubscriptions(in: scene); invoke.resolve() }
        catch { reject(invoke, error, fallback: "manage_subscriptions_failed") }
    }

    private func reject(_ invoke: Invoke, _ error: Error, fallback: String) {
        // Do not bridge Apple's error text/userInfo, which may include purchase data.
        invoke.reject((error as? IapFailure)?.rawValue ?? fallback)
    }

    private static func formatPeriod(_ period: Product.SubscriptionPeriod) -> String {
        switch period.unit {
        case .day: return "P\(period.value)D"
        case .week: return "P\(period.value)W"
        case .month: return "P\(period.value)M"
        case .year: return "P\(period.value)Y"
        @unknown default: return ""
        }
    }
}

@_cdecl("init_plugin_iap")
func initPlugin() -> Plugin {
    if #available(iOS 16.0, *) { return IapPlugin() }
    return Plugin()
}
