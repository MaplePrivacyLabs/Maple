import { afterEach, describe, expect, test } from "bun:test";
import { memo } from "react";
import { act, create, type ReactTestRenderer } from "react-test-renderer";

import type { BillingStatus } from "@/billing/billingApi";
import type {
  BillingState,
  ModelState,
  SelectedProjectState,
  SidebarSearchState
} from "./LocalStateContextDef";
import {
  DEFAULT_MODEL_ID,
  LocalStateProvider,
  POWERFUL_MODEL_ALIAS,
  SELECTED_MODEL_RESET_AT_KEY
} from "./LocalStateContext";
import {
  useBillingState,
  useModelState,
  useSelectedProjectState,
  useSidebarSearchState
} from "./useLocalState";

class CountingMemoryStorage {
  private readonly values = new Map<string, string>();
  readonly reads = new Map<string, number>();

  getItem(key: string): string | null {
    this.reads.set(key, (this.reads.get(key) ?? 0) + 1);
    return this.values.get(key) ?? null;
  }

  setItem(key: string, value: string): void {
    this.values.set(key, value);
  }

  removeItem(key: string): void {
    this.values.delete(key);
  }
}

type DomainSnapshots = {
  model?: ModelState;
  billing?: BillingState;
  sidebarSearch?: SidebarSearchState;
  selectedProject?: SelectedProjectState;
};

type RenderCounts = {
  model: number;
  billing: number;
  sidebarSearch: number;
  selectedProject: number;
};

type ProbeProps = {
  snapshots: DomainSnapshots;
  counts: RenderCounts;
};

const ModelProbe = memo(function ModelProbe({ snapshots, counts }: ProbeProps) {
  counts.model += 1;
  snapshots.model = useModelState();
  return null;
});

const BillingProbe = memo(function BillingProbe({ snapshots, counts }: ProbeProps) {
  counts.billing += 1;
  snapshots.billing = useBillingState();
  return null;
});

const SidebarSearchProbe = memo(function SidebarSearchProbe({ snapshots, counts }: ProbeProps) {
  counts.sidebarSearch += 1;
  snapshots.sidebarSearch = useSidebarSearchState();
  return null;
});

const SelectedProjectProbe = memo(function SelectedProjectProbe({ snapshots, counts }: ProbeProps) {
  counts.selectedProject += 1;
  snapshots.selectedProject = useSelectedProjectState();
  return null;
});

function DomainProbes(props: ProbeProps) {
  return (
    <>
      <ModelProbe {...props} />
      <BillingProbe {...props} />
      <SidebarSearchProbe {...props} />
      <SelectedProjectProbe {...props} />
    </>
  );
}

function createCounts(): RenderCounts {
  return { model: 0, billing: 0, sidebarSearch: 0, selectedProject: 0 };
}

function expectRenderDelta(
  counts: RenderCounts,
  before: RenderCounts,
  expected: Partial<RenderCounts>
) {
  expect({
    model: counts.model - before.model,
    billing: counts.billing - before.billing,
    sidebarSearch: counts.sidebarSearch - before.sidebarSearch,
    selectedProject: counts.selectedProject - before.selectedProject
  }).toEqual({
    model: expected.model ?? 0,
    billing: expected.billing ?? 0,
    sidebarSearch: expected.sidebarSearch ?? 0,
    selectedProject: expected.selectedProject ?? 0
  });
}

function freeBillingStatus(): BillingStatus {
  return {
    is_subscribed: false,
    stripe_customer_id: null,
    product_id: "free",
    product_name: "Free",
    subscription_status: "inactive",
    current_period_end: null,
    can_chat: true,
    chats_remaining: null,
    payment_provider: null,
    total_tokens: null,
    used_tokens: null,
    usage_reset_date: null
  };
}

function proBillingStatus(): BillingStatus {
  return {
    ...freeBillingStatus(),
    is_subscribed: true,
    product_id: "pro",
    product_name: "Pro",
    subscription_status: "active",
    payment_provider: "stripe"
  };
}

function stampSelectedModelReset(storage: CountingMemoryStorage, at = "2026-01-01T00:00:00.000Z") {
  storage.setItem(SELECTED_MODEL_RESET_AT_KEY, at);
}

function expectIsoTimestamp(value: string | null) {
  expect(value).toBeTruthy();
  expect(Number.isNaN(Date.parse(value ?? ""))).toBe(false);
}

describe("LocalStateProvider", () => {
  let renderer: ReactTestRenderer | null = null;

  afterEach(() => {
    if (renderer) {
      act(() => renderer?.unmount());
      renderer = null;
    }
  });

  test("isolates updates to the domain that changed", () => {
    const storage = new CountingMemoryStorage();
    const snapshots: DomainSnapshots = {};
    const counts = createCounts();

    act(() => {
      renderer = create(
        <LocalStateProvider storage={storage}>
          <DomainProbes snapshots={snapshots} counts={counts} />
        </LocalStateProvider>
      );
    });

    let before = { ...counts };
    act(() => snapshots.model?.setModel(POWERFUL_MODEL_ALIAS));
    expectRenderDelta(counts, before, { model: 1 });

    before = { ...counts };
    act(() => snapshots.billing?.setBillingStatus(freeBillingStatus()));
    expectRenderDelta(counts, before, { billing: 1 });

    before = { ...counts };
    act(() => {
      snapshots.sidebarSearch?.setSearchQuery("maple");
      snapshots.sidebarSearch?.setIsSearchVisible(true);
    });
    expectRenderDelta(counts, before, { sidebarSearch: 1 });

    before = { ...counts };
    act(() => snapshots.selectedProject?.setSelectedProjectId("project-1"));
    expectRenderDelta(counts, before, { selectedProject: 1 });

    before = { ...counts };
    act(() => snapshots.selectedProject?.setSelectedProjectId("project-1"));
    expectRenderDelta(counts, before, {});
  });

  test("keeps Quick as the default model when a paid plan is loaded", () => {
    const storage = new CountingMemoryStorage();
    const snapshots: DomainSnapshots = {};
    const counts = createCounts();

    act(() => {
      renderer = create(
        <LocalStateProvider storage={storage}>
          <DomainProbes snapshots={snapshots} counts={counts} />
        </LocalStateProvider>
      );
    });

    expect(snapshots.model?.model).toBe(DEFAULT_MODEL_ID);

    const before = { ...counts };
    act(() => snapshots.billing?.setBillingStatus(proBillingStatus()));

    expectRenderDelta(counts, before, { billing: 1 });
    expect(snapshots.model?.model).toBe(DEFAULT_MODEL_ID);
    expect(storage.getItem("selectedModel")).toBeNull();
    expectIsoTimestamp(storage.getItem(SELECTED_MODEL_RESET_AT_KEY));
  });

  test("clears a stored model once, then preserves later sticky choices", () => {
    const storage = new CountingMemoryStorage();
    const snapshots: DomainSnapshots = {};
    const counts = createCounts();
    storage.setItem("selectedModel", POWERFUL_MODEL_ALIAS);
    storage.setItem(
      "selectedModelMetadata",
      JSON.stringify({
        id: POWERFUL_MODEL_ALIAS,
        object: "model",
        created: 1,
        owned_by: "opensecret"
      })
    );

    act(() => {
      renderer = create(
        <LocalStateProvider storage={storage}>
          <DomainProbes snapshots={snapshots} counts={counts} />
        </LocalStateProvider>
      );
    });

    expect(snapshots.model?.model).toBe(DEFAULT_MODEL_ID);
    expect(storage.getItem("selectedModel")).toBeNull();
    expect(storage.getItem("selectedModelMetadata")).toBeNull();
    const resetAt = storage.getItem(SELECTED_MODEL_RESET_AT_KEY);
    expectIsoTimestamp(resetAt);

    act(() => renderer?.unmount());
    renderer = null;

    storage.setItem("selectedModel", POWERFUL_MODEL_ALIAS);

    act(() => {
      renderer = create(
        <LocalStateProvider storage={storage}>
          <DomainProbes snapshots={snapshots} counts={counts} />
        </LocalStateProvider>
      );
    });

    expect(snapshots.model?.model).toBe(POWERFUL_MODEL_ALIAS);
    expect(storage.getItem("selectedModel")).toBe(POWERFUL_MODEL_ALIAS);
    expect(storage.getItem(SELECTED_MODEL_RESET_AT_KEY)).toBe(resetAt);
  });

  test("keeps a stickied model when a paid plan is loaded", () => {
    const storage = new CountingMemoryStorage();
    const snapshots: DomainSnapshots = {};
    const counts = createCounts();
    stampSelectedModelReset(storage);
    storage.setItem("selectedModel", POWERFUL_MODEL_ALIAS);

    act(() => {
      renderer = create(
        <LocalStateProvider storage={storage}>
          <DomainProbes snapshots={snapshots} counts={counts} />
        </LocalStateProvider>
      );
    });

    expect(snapshots.model?.model).toBe(POWERFUL_MODEL_ALIAS);

    act(() => snapshots.billing?.setBillingStatus(proBillingStatus()));

    expect(snapshots.model?.model).toBe(POWERFUL_MODEL_ALIAS);
    expect(storage.getItem("selectedModel")).toBe(POWERFUL_MODEL_ALIAS);
  });

  test("ignores leftover paid-default cache when no model is stickied", () => {
    const storage = new CountingMemoryStorage();
    const snapshots: DomainSnapshots = {};
    const counts = createCounts();
    storage.setItem("paidDefaultsApplied", new Date().toISOString());
    storage.setItem("cachedBillingStatus", JSON.stringify(proBillingStatus()));

    act(() => {
      renderer = create(
        <LocalStateProvider storage={storage}>
          <DomainProbes snapshots={snapshots} counts={counts} />
        </LocalStateProvider>
      );
    });

    expect(snapshots.model?.model).toBe(DEFAULT_MODEL_ID);
    expectIsoTimestamp(storage.getItem(SELECTED_MODEL_RESET_AT_KEY));
  });

  test("preserves an in-memory model choice when storage is unavailable", () => {
    const snapshots: DomainSnapshots = {};
    const counts = createCounts();

    act(() => {
      renderer = create(
        <LocalStateProvider storage={null}>
          <DomainProbes snapshots={snapshots} counts={counts} />
        </LocalStateProvider>
      );
    });

    act(() => {
      snapshots.model?.setModel("user-selected-model", {
        id: "user-selected-model",
        object: "model",
        created: 1,
        owned_by: "opensecret"
      });
      snapshots.billing?.setBillingStatus(proBillingStatus());
    });

    expect(snapshots.model?.model).toBe("user-selected-model");
  });

  test("persists model choices without making a no-op selection sticky", () => {
    const storage = new CountingMemoryStorage();
    const snapshots: DomainSnapshots = {};
    const counts = createCounts();

    act(() => {
      renderer = create(
        <LocalStateProvider storage={storage}>
          <DomainProbes snapshots={snapshots} counts={counts} />
        </LocalStateProvider>
      );
    });

    act(() => snapshots.model?.setModel(DEFAULT_MODEL_ID));
    expect(storage.getItem("selectedModel")).toBeNull();
    expectIsoTimestamp(storage.getItem(SELECTED_MODEL_RESET_AT_KEY));

    act(() => {
      snapshots.model?.setModel("user-selected-model", {
        id: "user-selected-model",
        object: "model",
        created: 1,
        owned_by: "opensecret"
      });
      snapshots.model?.setModel(DEFAULT_MODEL_ID);
    });

    expect(snapshots.model?.model).toBe(DEFAULT_MODEL_ID);
    expect(storage.getItem("selectedModel")).toBe(DEFAULT_MODEL_ID);
    expect(storage.getItem("selectedModelMetadata")).toBeNull();
  });

  test("reads cached model state only during lazy provider initialization", () => {
    const storage = new CountingMemoryStorage();
    const snapshots: DomainSnapshots = {};
    const counts = createCounts();
    stampSelectedModelReset(storage);
    storage.setItem("selectedModel", "cached-model");
    storage.setItem(
      "selectedModelMetadata",
      JSON.stringify({
        id: "cached-model",
        object: "model",
        created: 1,
        owned_by: "opensecret"
      })
    );

    act(() => {
      renderer = create(
        <LocalStateProvider storage={storage}>
          <DomainProbes snapshots={snapshots} counts={counts} />
        </LocalStateProvider>
      );
    });

    const initialReads = new Map(storage.reads);
    expect([...initialReads.values()].reduce((sum, reads) => sum + reads, 0)).toBeGreaterThan(0);
    expect(snapshots.model?.model).toBe("cached-model");

    act(() => {
      snapshots.sidebarSearch?.setSearchQuery("persist across routes");
      snapshots.selectedProject?.setSelectedProjectId("project-2");
    });

    act(() => {
      renderer?.update(
        <LocalStateProvider storage={storage}>
          <DomainProbes snapshots={snapshots} counts={counts} />
          <span>another route</span>
        </LocalStateProvider>
      );
    });

    expect(storage.reads).toEqual(initialReads);
    expect(snapshots.sidebarSearch?.searchQuery).toBe("persist across routes");
    expect(snapshots.selectedProject?.selectedProjectId).toBe("project-2");
  });
  test("cached model metadata cannot restore an affirmative account grant", () => {
    const storage = new CountingMemoryStorage();
    const snapshots: DomainSnapshots = {};
    storage.setItem("selectedModel", "uncensored");
    storage.setItem(SELECTED_MODEL_RESET_AT_KEY, new Date().toISOString());
    storage.setItem(
      "selectedModelMetadata",
      JSON.stringify({
        id: "uncensored",
        object: "model",
        created: 0,
        owned_by: "maple",
        access: "pro",
        accessPolicy: { allowed: true, eligible_plans: ["max", "teams"], reason: null }
      })
    );
    act(() => {
      renderer = create(
        <LocalStateProvider storage={storage}>
          <DomainProbes snapshots={snapshots} counts={createCounts()} />
        </LocalStateProvider>
      );
    });
    expect(snapshots.model?.availableModels[0].accessPolicy?.allowed).toBe(false);
    act(() =>
      snapshots.model?.setModel("uncensored", {
        id: "uncensored",
        object: "model",
        created: 0,
        owned_by: "maple",
        access: "pro",
        accessPolicy: { allowed: true, eligible_plans: ["max", "teams"], reason: null }
      })
    );
    expect(snapshots.model?.availableModels[0].accessPolicy?.allowed).toBe(true);
    expect(JSON.parse(storage.getItem("selectedModelMetadata")!).accessPolicy.allowed).toBe(false);
  });
});
