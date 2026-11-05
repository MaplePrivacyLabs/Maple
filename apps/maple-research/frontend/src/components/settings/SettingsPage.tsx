import type { ReactNode } from "react";
import { cn } from "@/utils/utils";

type SettingsPageProps = {
  title: string;
  description: string;
  children: ReactNode;
  actions?: ReactNode;
  /** Wider column for pages that carry charts and tables. */
  wide?: boolean;
  className?: string;
};

export function SettingsPage({
  title,
  description,
  children,
  actions,
  wide = false,
  className
}: SettingsPageProps) {
  return (
    <div
      className={cn(
        "mx-auto w-full px-4 py-7 sm:px-8 sm:py-10 lg:px-10",
        wide ? "max-w-5xl" : "max-w-4xl",
        className
      )}
    >
      <header className="mb-7 flex flex-col gap-4 border-b border-border/70 pb-6 sm:flex-row sm:items-start sm:justify-between">
        <div className="min-w-0">
          <h1 className="text-2xl font-semibold tracking-tight text-foreground">{title}</h1>
          <p className="mt-1.5 max-w-2xl text-sm leading-relaxed text-muted-foreground">
            {description}
          </p>
        </div>
        {actions && <div className="shrink-0">{actions}</div>}
      </header>
      <div className="space-y-6">{children}</div>
    </div>
  );
}

type SettingsSectionProps = {
  title?: string;
  description?: string;
  children: ReactNode;
  className?: string;
  tone?: "default" | "danger";
};

export function SettingsSection({
  title,
  description,
  children,
  className,
  tone = "default"
}: SettingsSectionProps) {
  return (
    <section
      className={cn(
        "rounded-xl border bg-card p-4 text-card-foreground shadow-sm sm:p-6",
        tone === "danger" && "border-destructive/40",
        className
      )}
    >
      {(title || description) && (
        <div className="mb-5">
          {title && <h2 className="text-base font-semibold">{title}</h2>}
          {description && (
            <p className="mt-1 text-sm leading-relaxed text-muted-foreground">{description}</p>
          )}
        </div>
      )}
      {children}
    </section>
  );
}
