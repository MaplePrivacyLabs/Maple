import type { ReactNode } from "react";
import { AlertCircle, Layers, Loader2, RotateCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/utils/utils";

export function ScopeNote() {
  return (
    <div className="flex items-start gap-2.5 rounded-xl border border-border/70 bg-muted/40 px-4 py-3 text-sm leading-relaxed text-muted-foreground">
      <Layers className="mt-0.5 h-4 w-4 shrink-0 text-foreground/70" />
      <p>
        <span className="font-medium text-foreground">Your whole account.</span> Includes Maple
        Research and Maple Work on every device, plus requests made with your API keys. It is not
        limited to this device.
      </p>
    </div>
  );
}

export function StatTile({
  label,
  value,
  sub,
  children,
  className
}: {
  label: string;
  value?: ReactNode;
  sub?: ReactNode;
  children?: ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("flex min-w-0 flex-col rounded-xl border bg-card p-4 shadow-sm", className)}>
      <p className="text-xs font-medium text-muted-foreground">{label}</p>
      {value !== undefined && (
        <p className="mt-1.5 text-2xl font-semibold tracking-tight text-foreground">{value}</p>
      )}
      {children}
      {sub && <p className="mt-auto pt-1.5 text-xs text-muted-foreground">{sub}</p>}
    </div>
  );
}

export function SectionHeader({
  title,
  description,
  right
}: {
  title: string;
  description?: ReactNode;
  right?: ReactNode;
}) {
  return (
    <div className="mb-5 flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
      <div>
        <h2 className="text-base font-semibold">{title}</h2>
        {description && (
          <p className="mt-1 text-sm leading-relaxed text-muted-foreground">{description}</p>
        )}
      </div>
      {right && <div className="shrink-0">{right}</div>}
    </div>
  );
}

export function Segmented<T extends string>({
  value,
  onChange,
  options,
  label
}: {
  value: T;
  onChange: (value: T) => void;
  options: Array<{ value: T; label: string }>;
  label: string;
}) {
  return (
    <div
      role="group"
      aria-label={label}
      className="inline-flex rounded-lg border border-border/70 bg-background p-0.5"
    >
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          aria-pressed={value === o.value}
          onClick={() => onChange(o.value)}
          className={cn(
            "rounded-md px-2.5 py-1 text-xs font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
            value === o.value
              ? "bg-[hsl(var(--maple-primary-container))] text-foreground"
              : "text-muted-foreground hover:text-foreground"
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

export function LoadingBlock({ label, height }: { label: string; height?: number }) {
  return (
    <div
      className="flex items-center justify-center gap-2 py-8 text-sm text-muted-foreground"
      style={height ? { minHeight: height } : undefined}
      role="status"
    >
      <Loader2 className="h-4 w-4 animate-spin" />
      {label}
    </div>
  );
}

export function ErrorBlock({ message, onRetry }: { message: string; onRetry?: () => void }) {
  return (
    <div
      role="alert"
      className="flex flex-col gap-3 rounded-lg border border-destructive/30 bg-destructive/5 px-4 py-3 text-sm sm:flex-row sm:items-center sm:justify-between"
    >
      <span className="flex items-center gap-2 text-foreground">
        <AlertCircle className="h-4 w-4 shrink-0 text-destructive" />
        {message}
      </span>
      {onRetry && (
        <Button type="button" variant="outline" size="sm" className="h-8" onClick={onRetry}>
          <RotateCw className="mr-2 h-3.5 w-3.5" />
          Try again
        </Button>
      )}
    </div>
  );
}
