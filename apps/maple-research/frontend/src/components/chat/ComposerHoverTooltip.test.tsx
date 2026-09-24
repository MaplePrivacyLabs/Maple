import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import { act, create, type ReactTestRenderer } from "react-test-renderer";
import { Tooltip, TooltipProvider } from "@/components/ui/tooltip";
import { DropdownMenu, DropdownMenuTrigger } from "@/components/ui/dropdown-menu";
import { ComposerHoverTooltip } from "./ComposerHoverTooltip";

const originalWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
const originalDocument = Object.getOwnPropertyDescriptor(globalThis, "document");
const originalSetTimeout = globalThis.setTimeout;
const originalClearTimeout = globalThis.clearTimeout;

describe("ComposerHoverTooltip", () => {
  let renderer: ReactTestRenderer | null = null;
  let focused: boolean;
  let now: number;
  let nextTimer: number;
  const timers = new Map<number, { at: number; run: () => void }>();

  beforeEach(() => {
    focused = true;
    now = 0;
    nextTimer = 0;
    globalThis.setTimeout = ((run: () => void, delay = 0) => {
      timers.set(++nextTimer, { at: now + delay, run });
      return nextTimer;
    }) as unknown as typeof setTimeout;
    globalThis.clearTimeout = ((id: number) => timers.delete(id)) as unknown as typeof clearTimeout;
    Object.defineProperty(globalThis, "window", {
      configurable: true,
      value: Object.assign(new EventTarget(), {
        setTimeout: globalThis.setTimeout,
        clearTimeout: globalThis.clearTimeout
      })
    });
    Object.defineProperty(globalThis, "document", {
      configurable: true,
      value: Object.assign(new EventTarget(), { hasFocus: () => focused })
    });
  });

  afterEach(() => {
    act(() => renderer?.unmount());
    renderer = null;
    timers.clear();
    globalThis.setTimeout = originalSetTimeout;
    globalThis.clearTimeout = originalClearTimeout;
    for (const [key, descriptor] of [
      ["window", originalWindow],
      ["document", originalDocument]
    ] as const) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor);
      else Reflect.deleteProperty(globalThis, key);
    }
  });

  function advance(milliseconds: number) {
    act(() => {
      now += milliseconds;
      for (const [id, timer] of [...timers]) {
        if (timer.at <= now) {
          timers.delete(id);
          timer.run();
        }
      }
    });
  }

  function render(enabled = true) {
    const tree = (
      <TooltipProvider>
        <ComposerHoverTooltip label="Add files" enabled={enabled}>
          <button type="button">Add files</button>
        </ComposerHoverTooltip>
      </TooltipProvider>
    );
    act(() => {
      if (renderer) renderer.update(tree);
      else renderer = create(tree);
    });
  }

  function fire(name: string, properties = {}) {
    const event = {
      pointerType: "mouse",
      buttons: 0,
      button: 0,
      ctrlKey: false,
      defaultPrevented: false,
      preventDefault() {
        this.defaultPrevented = true;
      },
      ...properties
    };
    act(() => renderer!.root.findByType("button").props[name](event));
    return event;
  }

  function isOpen() {
    return renderer!.root.findByType(Tooltip).props.open;
  }

  test("waits 500 ms on every hover, stays visible, and restarts after leaving an edge", () => {
    render();
    fire("onPointerMove");
    advance(499);
    expect(isOpen()).toBe(false);
    fire("onPointerLeave");
    fire("onPointerMove");
    advance(499);
    expect(isOpen()).toBe(false);
    advance(1);
    expect(isOpen()).toBe(true);
    advance(2000);
    expect(isOpen()).toBe(true);
    fire("onPointerLeave");
    expect(isOpen()).toBe(false);
    fire("onPointerMove");
    advance(499);
    expect(isOpen()).toBe(false);
    advance(1);
    expect(isOpen()).toBe(true);
  });

  test("does not open from focus, touch, a held mouse button, or an inactive window", () => {
    render();
    fire("onFocus");
    fire("onPointerMove", { pointerType: "touch" });
    fire("onPointerMove", { buttons: 1 });
    focused = false;
    fire("onPointerMove");
    advance(1000);
    expect(isOpen()).toBe(false);
  });

  test.each(["onPointerLeave", "onPointerDown", "onBlur", "onClick"])(
    "%s cancels pending hover feedback",
    (event) => {
      render();
      fire("onPointerMove");
      advance(250);
      fire(event);
      advance(1000);
      expect(isOpen()).toBe(false);
    }
  );

  test("window blur cancels pending feedback and focus alone does not reopen it", () => {
    render();
    fire("onPointerMove");
    act(() => {
      window.dispatchEvent(new Event("blur"));
    });
    fire("onFocus");
    advance(1000);
    expect(isOpen()).toBe(false);
  });

  test("honors Radix dismissal requests and closes when the control becomes unavailable", () => {
    render();
    fire("onPointerMove");
    advance(500);
    act(() => renderer!.root.findByType(Tooltip).props.onOpenChange(false));
    expect(isOpen()).toBe(false);
    fire("onPointerMove");
    advance(500);
    expect(isOpen()).toBe(true);
    render(false);
    expect(isOpen()).toBe(false);
    render(true);
    advance(1000);
    expect(isOpen()).toBe(false);
  });

  test("preserves menu opening and the button click handler", () => {
    const onClick = mock(() => {});
    act(() => {
      renderer = create(
        <TooltipProvider>
          <DropdownMenu>
            <ComposerHoverTooltip label="Add files">
              <DropdownMenuTrigger asChild>
                <button type="button" onClick={onClick}>
                  Add files
                </button>
              </DropdownMenuTrigger>
            </ComposerHoverTooltip>
          </DropdownMenu>
        </TooltipProvider>
      );
    });
    fire("onPointerMove");
    advance(500);
    fire("onPointerDown");
    expect(renderer!.root.findByType("button").props["aria-expanded"]).toBe(true);
    expect(isOpen()).toBe(false);
    fire("onClick");
    expect(onClick).toHaveBeenCalledTimes(1);
  });
});
