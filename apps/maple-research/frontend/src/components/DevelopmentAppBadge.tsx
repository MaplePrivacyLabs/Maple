import { mapleAppVariant } from "@/config/mapleAppVariant";

/** Build identity stays visible through sign-in, account changes, and modal routes. */
export function DevelopmentAppBadge() {
  if (mapleAppVariant() !== "dev") return null;
  return (
    <div
      className="pointer-events-none fixed left-1/2 z-[2147483647] -translate-x-1/2 rounded-b-md bg-amber-300 px-3 py-0.5 text-[11px] font-bold tracking-wide text-slate-950 shadow-sm"
      style={{ top: "env(safe-area-inset-top, 0px)" }}
      aria-label="Maple Dev — development environment"
    >
      MAPLE DEV
    </div>
  );
}
