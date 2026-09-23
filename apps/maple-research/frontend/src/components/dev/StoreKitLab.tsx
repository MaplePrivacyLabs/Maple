import { useCallback, useEffect, useRef, useState } from "react";
import "../../index.css";
import {
  storeKit,
  StoreKitRecovery,
  StoreKitRecoveryError,
  type SignedStoreKitTransaction,
  type StoreKitAcknowledgement,
  type StoreKitProduct
} from "@/services/storeKitService";

const productIds = [
  "cloud.opensecret.maple.pro.monthly",
  "cloud.opensecret.maple.max.monthly",
  "cloud.opensecret.maple.pro.yearly"
];
const fixtureOrigin = "http://127.0.0.1:38863";

function localSigningMetadata(
  transaction: SignedStoreKitTransaction,
  certificateSource: "verified_purchase" | "verified_recovery"
) {
  // Diagnostic bootstrap only: StoreKit has already verified this transaction.
  // Export only the public signing certificate, never the signed transaction.
  // The fixture server independently pins and verifies the certificate later.
  const decode = (part: string) => JSON.parse(atob(part.replace(/-/g, "+").replace(/_/g, "/")));
  try {
    const [header, payload] = transaction.jws.split(".");
    const environment = decode(payload).environment;
    const certificate = decode(header).x5c?.[0];
    return environment === "Xcode" && typeof certificate === "string"
      ? { environment, certificateSource, localSigningCertificate: certificate }
      : {
          environment: "unexpected",
          certificateSource: undefined,
          localSigningCertificate: undefined
        };
  } catch {
    return {
      environment: "unreadable",
      certificateSource: undefined,
      localSigningCertificate: undefined
    };
  }
}

async function fixtureRequest<T>(path: string, body?: unknown): Promise<T> {
  const response = await fetch(`${fixtureOrigin}${path}`, {
    method: body === undefined ? "GET" : "POST",
    headers: { "Content-Type": "application/json", "X-Maple-StoreKit-Fixture": "1" },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(10_000)
  });
  if (!response.ok) throw new Error(`fixture_http_${response.status}`);
  return response.json();
}

/** A fixture-only screen loaded by main.tsx after the native simulator guard. */
export default function StoreKitLab() {
  const [products, setProducts] = useState<StoreKitProduct[]>([]);
  const [busy, setBusy] = useState(false);
  const [showCertificate, setShowCertificate] = useState(false);
  const [status, setStatus] = useState<Record<string, unknown>>({
    purchaseStatus: "idle",
    unfinishedCount: 0,
    acknowledgedCount: 0,
    listenerCount: 0,
    automaticRecovery: false
  });
  const recoveryRef = useRef<StoreKitRecovery | null>(null);
  const automaticRef = useRef(false);
  const mountedRef = useRef(false);

  const update = useCallback((values: Record<string, unknown>) => {
    if (mountedRef.current) setStatus((current) => ({ ...current, ...values }));
  }, []);

  const refreshTransactions = useCallback(async () => {
    const result = await storeKit.getSignedTransactions();
    const firstTransaction = result.transactions[0];
    update({
      ...(firstTransaction ? localSigningMetadata(firstTransaction, "verified_recovery") : {}),
      transactionCount: result.transactions.length,
      unfinishedCount: result.unfinishedTransactionIds.length,
      unfinishedTransactionIds: result.unfinishedTransactionIds,
      signedTransactionsPresent: result.transactions.every((tx) => tx.jws.split(".").length === 3)
    });
  }, [update]);

  const withTransactionRefresh = useCallback(
    async (operation: () => Promise<void>) => {
      let failed = false;
      let failure: unknown;
      try {
        await operation();
      } catch (error) {
        failed = true;
        failure = error;
      }
      try {
        await refreshTransactions();
      } catch (error) {
        // Preserve the recovery error if refreshing also fails.
        if (!failed) {
          failed = true;
          failure = error;
        }
      }
      if (failed) throw failure;
    },
    [refreshTransactions]
  );

  const recordAcknowledgement = useCallback((ack: StoreKitAcknowledgement) => {
    if (!mountedRef.current) return;
    setStatus((current) => ({
      ...current,
      acknowledgedCount: Number(current.acknowledgedCount) + 1,
      acknowledgedTransactionId: ack.acknowledged_transaction_id,
      paymentProvider: ack.payment_provider
    }));
  }, []);

  useEffect(() => {
    mountedRef.current = true;
    const recovery = new StoreKitRecovery(storeKit, async (signed_transaction) => {
      const ack = await fixtureRequest<StoreKitAcknowledgement>(
        "/v1/maple/subscription/apple/transactions",
        { signed_transaction }
      );
      return ack;
    });
    recoveryRef.current = recovery;
    let disposed = false;
    const unlisteners: (() => void)[] = [];
    const register = async () => {
      const transactionListener = await storeKit.onTransaction((transaction) => {
        if (disposed) return;
        setStatus((current) => ({
          ...current,
          listenerCount: Number(current.listenerCount) + 1,
          listenerTransactionId: transaction.transactionId,
          listenerJwsPresent: transaction.jws.split(".").length === 3
        }));
        if (automaticRef.current) {
          void withTransactionRefresh(async () => {
            const ack = await recovery.acknowledge(transaction);
            if (!disposed) recordAcknowledgement(ack);
          }).catch(() => update({ error: "listener_recovery_pending" }));
        }
      });
      if (disposed) transactionListener();
      else unlisteners.push(transactionListener);
      const storefrontListener = await storeKit.onStorefront((storefront) =>
        update({ storefront })
      );
      if (disposed) storefrontListener();
      else unlisteners.push(storefrontListener);
      if (!disposed) update({ listenersReady: true });
    };
    void register().catch(() => update({ error: "listener_registration_failed" }));
    return () => {
      disposed = true;
      mountedRef.current = false;
      recovery.dispose();
      recoveryRef.current = null;
      for (const unlisten of unlisteners) unlisten();
    };
  }, [recordAcknowledgement, update, withTransactionRefresh]);

  async function run(operation: () => Promise<void>) {
    setBusy(true);
    update({ error: null });
    try {
      await operation();
    } catch (error) {
      // Native errors and JWS are deliberately not reflected into the UI/logs.
      const message = error instanceof Error ? error.message : "native_operation_failed";
      update({
        error: /^(fixture_http_\d{3}|storekit_[a-z_]+)$/.test(message)
          ? message
          : "operation_failed"
      });
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  }

  async function purchase() {
    await withTransactionRefresh(async () => {
      const token = await fixtureRequest<{ app_account_token: string }>(
        "/v1/maple/subscription/apple/account-token"
      );
      const result = await storeKit.purchase(productIds[0], token.app_account_token);
      update({ purchaseStatus: result.status });
      if (result.status === "success") {
        const tx: SignedStoreKitTransaction = result.transaction;
        update({
          ...localSigningMetadata(tx, "verified_purchase"),
          purchaseTransactionId: tx.transactionId,
          purchaseJwsPresent: tx.jws.split(".").length === 3
        });
        if (automaticRef.current && recoveryRef.current) {
          recordAcknowledgement(await recoveryRef.current.acknowledge(tx));
        }
      }
    });
  }

  async function recoverPurchases(restore: boolean) {
    await withTransactionRefresh(async () => {
      try {
        const recovery = recoveryRef.current!;
        const acknowledgements = await (restore ? recovery.restore() : recovery.recover());
        acknowledgements.forEach(recordAcknowledgement);
      } catch (error) {
        if (error instanceof StoreKitRecoveryError) {
          error.acknowledgements.forEach(recordAcknowledgement);
        }
        throw error;
      }
    });
  }

  const { localSigningCertificate, ...statusBeforeCertificate } = status;
  const buttonClass = "rounded border border-gray-500 px-3 py-2 text-left disabled:opacity-50";
  if (showCertificate) {
    return (
      <main className="h-dvh bg-white px-3 pb-3 pt-14 text-black">
        <button className={buttonClass} onClick={() => setShowCertificate(false)}>
          Back
        </button>
        <h1 className="mt-3 text-sm font-semibold">Public Xcode signing certificate</h1>
        <pre
          aria-label="Public Xcode signing certificate"
          className="mt-2 whitespace-pre-wrap break-all font-mono text-[11px] leading-[13px]"
        >
          {typeof localSigningCertificate === "string"
            ? localSigningCertificate
            : "No public certificate available"}
        </pre>
      </main>
    );
  }
  return (
    <main className="h-dvh overflow-y-auto bg-white px-5 pb-8 pt-16 text-black">
      <h1 className="text-xl font-semibold">StoreKit local experiment</h1>
      <p className="my-2 text-sm">Xcode fixtures only. No real charges or Maple plan access.</p>
      <div className="my-4 grid grid-cols-2 gap-2">
        <button
          className={buttonClass}
          disabled={busy}
          onClick={() =>
            void run(async () => {
              const loaded = await storeKit.getProducts(productIds);
              setProducts(loaded);
              update({
                productIDs: loaded.map((product) => product.productId),
                storefront: await storeKit.getStorefront()
              });
            })
          }
        >
          Load products
        </button>
        <button
          className={buttonClass}
          disabled={busy || products.length === 0}
          onClick={() => void run(purchase)}
        >
          Purchase Pro
        </button>
        <button
          className={buttonClass}
          disabled={busy}
          onClick={() => void run(refreshTransactions)}
        >
          List transactions
        </button>
        <button
          className={buttonClass}
          disabled={busy}
          onClick={() => void run(() => recoverPurchases(false))}
        >
          Recover purchases
        </button>
        <button
          className={buttonClass}
          disabled={busy}
          onClick={() => void run(() => recoverPurchases(true))}
        >
          Restore purchases
        </button>
        <button
          className={buttonClass}
          disabled={busy}
          onClick={() => void run(() => storeKit.manageSubscriptions())}
        >
          Manage subscriptions
        </button>
        <button
          className={buttonClass}
          disabled={busy}
          onClick={() => {
            automaticRef.current = !automaticRef.current;
            update({ automaticRecovery: automaticRef.current });
          }}
        >
          Toggle automatic recovery
        </button>
        <button
          className={buttonClass}
          disabled={busy || typeof localSigningCertificate !== "string"}
          onClick={() => setShowCertificate(true)}
        >
          Show public certificate
        </button>
      </div>
      {products.map((product) => (
        <p key={product.productId} className="text-sm">
          {product.title}: {product.formattedPrice}
        </p>
      ))}
      <pre
        role="status"
        data-testid="storekit-status"
        className="mt-4 whitespace-pre-wrap break-all text-xs"
      >
        {JSON.stringify({ ...statusBeforeCertificate, busy, localSigningCertificate }, null, 2)}
      </pre>
    </main>
  );
}
