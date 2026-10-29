import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { cn } from "@/utils/utils";
import { niceScale } from "./chartScale";

// Hand-drawn SVG charts for the Usage page. Colors come from the `.usage-viz`
// variables in index.css, so every chart must render inside that class.

function useWidth<T extends HTMLElement>() {
  const ref = useRef<T>(null);
  const [width, setWidth] = useState(0);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    setWidth(el.getBoundingClientRect().width);
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver((entries) => {
      for (const entry of entries) setWidth(entry.contentRect.width);
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  return [ref, width] as const;
}

function roundedTopPath(x: number, y: number, w: number, h: number, r: number): string {
  const rr = Math.max(0, Math.min(r, w / 2, h));
  return [
    `M${x},${y + h}`,
    `L${x},${y + rr}`,
    `Q${x},${y} ${x + rr},${y}`,
    `L${x + w - rr},${y}`,
    `Q${x + w},${y} ${x + w},${y + rr}`,
    `L${x + w},${y + h}`,
    "Z"
  ].join(" ");
}

export function Swatch({ color, className }: { color: string; className?: string }) {
  return (
    <span
      aria-hidden
      className={cn("inline-block h-2.5 w-2.5 shrink-0 rounded-[3px]", className)}
      style={{ background: color }}
    />
  );
}

export function Legend({
  items
}: {
  items: Array<{ key: string; label: string; color: string; value?: string }>;
}) {
  return (
    <div className="flex flex-wrap items-center gap-x-4 gap-y-1.5 text-xs text-muted-foreground">
      {items.map((item) => (
        <span key={item.key} className="inline-flex items-center gap-1.5">
          <Swatch color={item.color} />
          <span className="text-foreground/80">{item.label}</span>
          {item.value && <span className="tabular-nums">{item.value}</span>}
        </span>
      ))}
    </div>
  );
}

function ChartTooltip({ children }: { children: ReactNode }) {
  return (
    <div className="pointer-events-none min-w-[11rem] rounded-lg border bg-popover px-3 py-2 text-xs text-popover-foreground shadow-md">
      {children}
    </div>
  );
}

export type StackSeries = { key: string; label: string; color: string };
export type StackDatum = {
  key: string;
  tick: string;
  title: string;
  values: Record<string, number>;
  partial?: boolean;
  footer?: ReactNode;
};

export function StackedBars({
  data,
  series,
  height = 220,
  format,
  formatValue = format,
  unit,
  ariaLabel,
  markers = [],
  ranges = [],
  maxBarWidth = 26,
  refLine
}: {
  data: StackDatum[];
  series: StackSeries[];
  height?: number;
  /** Axis labels. */
  format: (n: number) => string;
  /** Tooltip values. */
  formatValue?: (n: number) => string;
  /** Tooltip total suffix, e.g. "credits". */
  unit: string;
  ariaLabel: string;
  markers?: Array<{ key: string; label: string }>;
  ranges?: Array<{ fromKey: string; toKey: string; label: string }>;
  maxBarWidth?: number;
  refLine?: { value: number; label: string };
}) {
  const [ref, width] = useWidth<HTMLDivElement>();
  const [hover, setHover] = useState<number | null>(null);

  const margin = {
    top: markers.length ? 22 : 8,
    right: 4,
    bottom: ranges.length ? 46 : 24,
    left: 44
  };
  const plotW = Math.max(0, width - margin.left - margin.right);
  const plotH = height - margin.top - margin.bottom;
  const totals = data.map((d) => series.reduce((s, ser) => s + (d.values[ser.key] ?? 0), 0));
  const { max: yMax, ticks } = niceScale(Math.max(...totals, refLine?.value ?? 0, 1) * 1.04);
  const band = data.length ? plotW / data.length : 0;
  const barW = Math.max(2, Math.min(maxBarWidth, band - Math.max(2, band * 0.28)));
  const y = (v: number) => margin.top + plotH - (v / yMax) * plotH;
  const labelEvery = Math.max(1, Math.ceil(data.length / Math.max(1, Math.floor(plotW / 58))));
  const active = hover !== null ? data[hover] : undefined;

  return (
    <div ref={ref} className="relative w-full select-none" style={{ height }}>
      {width > 0 && (
        <svg width={width} height={height} role="img" aria-label={ariaLabel}>
          {ticks.map((t) => (
            <g key={t}>
              <line
                x1={margin.left}
                x2={width - margin.right}
                y1={y(t)}
                y2={y(t)}
                stroke={t === 0 ? "var(--viz-axis)" : "var(--viz-grid)"}
                strokeWidth={1}
              />
              <text
                x={margin.left - 8}
                y={y(t)}
                dy="0.32em"
                textAnchor="end"
                className="tabular-nums"
                fontSize={10.5}
                fill="var(--viz-muted)"
              >
                {format(t)}
              </text>
            </g>
          ))}

          {data.map((d, i) => {
            const cx = margin.left + band * i + band / 2;
            const x = cx - barW / 2;
            let cursor = margin.top + plotH;
            const visible = series.filter((s) => (d.values[s.key] ?? 0) > 0);
            const dimmed = hover !== null && hover !== i;
            return (
              <g key={d.key} opacity={dimmed ? 0.5 : 1}>
                {visible.map((s, si) => {
                  const v = d.values[s.key] ?? 0;
                  const h = (v / yMax) * plotH;
                  const top = cursor - h;
                  const isTop = si === visible.length - 1;
                  const gap = si === 0 ? 0 : 2;
                  const segH = Math.max(0, h - gap);
                  cursor = top;
                  if (segH <= 0) return null;
                  return isTop ? (
                    <path key={s.key} d={roundedTopPath(x, top, barW, segH, 4)} fill={s.color} />
                  ) : (
                    <rect key={s.key} x={x} y={top} width={barW} height={segH} fill={s.color} />
                  );
                })}
                {d.partial && totals[i] > 0 && (
                  <text
                    x={cx}
                    y={cursor - 6}
                    textAnchor="middle"
                    fontSize={10}
                    fill="var(--viz-muted)"
                  >
                    so far
                  </text>
                )}
              </g>
            );
          })}

          {refLine && (
            <g>
              <line
                x1={margin.left}
                x2={width - margin.right}
                y1={y(refLine.value)}
                y2={y(refLine.value)}
                stroke="hsl(var(--foreground))"
                strokeOpacity={0.55}
              />
              <text
                x={width - margin.right}
                y={y(refLine.value) - 5}
                textAnchor="end"
                fontSize={10.5}
                fill="hsl(var(--foreground))"
                fillOpacity={0.75}
              >
                {refLine.label}
              </text>
            </g>
          )}

          {markers.map((m) => {
            const idx = data.findIndex((d) => d.key === m.key);
            if (idx < 0) return null;
            const mx = margin.left + band * idx;
            return (
              <g key={m.key}>
                <line
                  x1={mx}
                  x2={mx}
                  y1={margin.top - 6}
                  y2={margin.top + plotH}
                  stroke="hsl(var(--foreground))"
                  strokeOpacity={0.35}
                  strokeWidth={1}
                />
                <text
                  x={mx + 5}
                  y={margin.top - 8}
                  fontSize={10.5}
                  fill="hsl(var(--foreground))"
                  fillOpacity={0.7}
                >
                  {m.label}
                </text>
              </g>
            );
          })}

          {data.map((d, i) =>
            i % labelEvery === (data.length - 1) % labelEvery ? (
              <text
                key={d.key}
                x={margin.left + band * i + band / 2}
                y={margin.top + plotH + 15}
                textAnchor="middle"
                fontSize={10.5}
                fill="var(--viz-muted)"
              >
                {d.tick}
              </text>
            ) : null
          )}

          {ranges.map((r) => {
            const from = data.findIndex((d) => d.key === r.fromKey);
            const to = data.findIndex((d) => d.key === r.toKey);
            if (from < 0 || to < 0) return null;
            const x1 = margin.left + band * from + (band - barW) / 2;
            const x2 = margin.left + band * to + (band + barW) / 2;
            const ry = margin.top + plotH + 28;
            return (
              <g key={r.label}>
                <line
                  x1={x1}
                  x2={x2}
                  y1={ry}
                  y2={ry}
                  stroke="hsl(var(--foreground))"
                  strokeOpacity={0.55}
                  strokeWidth={3}
                  strokeLinecap="round"
                />
                <text
                  x={Math.min(x1, Math.max(margin.left, width - margin.right - 240))}
                  y={ry + 13}
                  fontSize={10.5}
                  fill="hsl(var(--foreground))"
                  fillOpacity={0.7}
                >
                  {r.label}
                </text>
              </g>
            );
          })}

          {data.map((d, i) => (
            <rect
              key={`hit-${d.key}`}
              x={margin.left + band * i}
              y={margin.top}
              width={band}
              height={plotH}
              fill="transparent"
              onMouseEnter={() => setHover(i)}
              onMouseLeave={() => setHover(null)}
            />
          ))}
        </svg>
      )}

      {hover !== null && active && (
        <div
          className="absolute z-10"
          style={(() => {
            const barRight = margin.left + band * hover + band / 2 + barW / 2 + 8;
            const barLeft = margin.left + band * hover + band / 2 - barW / 2 - 8;
            return barRight + 200 < width
              ? { left: barRight, top: margin.top }
              : { left: barLeft, top: margin.top, transform: "translateX(-100%)" };
          })()}
        >
          <ChartTooltip>
            <div className="mb-1.5 flex items-baseline justify-between gap-4">
              <span className="font-semibold">{active.title}</span>
              <span className="tabular-nums text-muted-foreground">
                {formatValue(totals[hover])} {unit}
              </span>
            </div>
            <div className="space-y-1">
              {[...series]
                .reverse()
                .filter((s) => (active.values[s.key] ?? 0) > 0)
                .map((s) => (
                  <div key={s.key} className="flex items-center justify-between gap-4">
                    <span className="inline-flex items-center gap-1.5 text-foreground/80">
                      <Swatch color={s.color} />
                      {s.label}
                    </span>
                    <span className="tabular-nums">{formatValue(active.values[s.key])}</span>
                  </div>
                ))}
            </div>
            {active.footer && (
              <div className="mt-1.5 border-t pt-1.5 text-muted-foreground">{active.footer}</div>
            )}
          </ChartTooltip>
        </div>
      )}
    </div>
  );
}

export function SplitBar({
  parts,
  height = 8
}: {
  parts: Array<{ key: string; label: string; value: number; color: string }>;
  height?: number;
}) {
  const total = parts.reduce((s, p) => s + p.value, 0);
  return (
    <div
      className="flex w-full gap-[2px] overflow-hidden rounded-full bg-[hsl(var(--sidebar-chrome-hover))]"
      style={{ height }}
    >
      {total > 0 &&
        parts
          .filter((p) => p.value > 0)
          .map((p) => (
            <div
              key={p.key}
              title={`${p.label}: ${Math.round((p.value / total) * 100)}%`}
              className="h-full first:rounded-l-full last:rounded-r-full"
              style={{ width: `${(p.value / total) * 100}%`, background: p.color }}
            />
          ))}
    </div>
  );
}

export function Meter({
  value,
  max,
  height = 8,
  over = false,
  label
}: {
  value: number;
  max: number;
  height?: number;
  over?: boolean;
  label: string;
}) {
  const pct = max > 0 ? Math.min(100, Math.max(0, (value / max) * 100)) : 0;
  return (
    <div
      className="w-full overflow-hidden rounded-full bg-[hsl(var(--sidebar-chrome-hover))]"
      style={{ height }}
      role="meter"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={max}
      aria-valuenow={Math.min(value, max)}
    >
      <div
        className="h-full rounded-full"
        style={{
          width: `${pct}%`,
          background: over
            ? "hsl(var(--maple-error))"
            : "linear-gradient(90deg, hsl(var(--maple-primary)), hsl(var(--maple-primary-strong)))"
        }}
      />
    </div>
  );
}

export function ShareBar({ value, max, color }: { value: number; max: number; color: string }) {
  const pct = max > 0 ? Math.max(1, (value / max) * 100) : 0;
  return (
    <div className="h-1.5 w-full overflow-hidden rounded-full bg-muted">
      <div className="h-full rounded-full" style={{ width: `${pct}%`, background: color }} />
    </div>
  );
}

export function CumulativeChart({
  actual,
  projected,
  totalDays,
  allowance,
  ticks,
  height = 220,
  format,
  allowanceLabel,
  overLabel
}: {
  /** Cumulative value per day from the window start through today. */
  actual: number[];
  projected: number;
  totalDays: number;
  allowance: number;
  ticks: Array<{ index: number; label: string }>;
  height?: number;
  format: (n: number) => string;
  allowanceLabel: string;
  overLabel: string;
}) {
  const [ref, width] = useWidth<HTMLDivElement>();
  if (actual.length === 0) return null;
  const margin = { top: 14, right: 64, bottom: 24, left: 48 };
  const plotW = Math.max(0, width - margin.left - margin.right);
  const plotH = height - margin.top - margin.bottom;
  const lastIdx = actual.length - 1;
  const today = actual[lastIdx];
  const { max: yMax, ticks: gridTicks } = niceScale(
    Math.max(projected, allowance, today, 1) * 1.06
  );
  const span = Math.max(1, totalDays - 1);
  const x = (i: number) => margin.left + (i / span) * plotW;
  const y = (v: number) => margin.top + plotH - (v / yMax) * plotH;
  const actualPath = actual
    .map((v, i) => `${i === 0 ? "M" : "L"}${x(i).toFixed(1)},${y(v).toFixed(1)}`)
    .join(" ");
  const areaPath = `${actualPath} L${x(lastIdx)},${y(0)} L${x(0)},${y(0)} Z`;
  const crossIdx = (() => {
    if (today >= allowance) return null;
    const perDay = (projected - today) / Math.max(1, totalDays - 1 - lastIdx);
    if (perDay <= 0) return null;
    const idx = lastIdx + (allowance - today) / perDay;
    return idx <= totalDays - 1 ? idx : null;
  })();

  return (
    <div ref={ref} className="relative w-full" style={{ height }}>
      {width > 0 && (
        <svg width={width} height={height} role="img" aria-label="Plan credits used this period">
          {gridTicks.map((t) => (
            <g key={t}>
              <line
                x1={margin.left}
                x2={margin.left + plotW}
                y1={y(t)}
                y2={y(t)}
                stroke={t === 0 ? "var(--viz-axis)" : "var(--viz-grid)"}
              />
              <text
                x={margin.left - 8}
                y={y(t)}
                dy="0.32em"
                textAnchor="end"
                fontSize={10.5}
                fill="var(--viz-muted)"
                className="tabular-nums"
              >
                {format(t)}
              </text>
            </g>
          ))}
          <rect
            x={margin.left}
            y={margin.top}
            width={plotW}
            height={Math.max(0, y(allowance) - margin.top)}
            fill="hsl(var(--maple-primary))"
            opacity={0.06}
          />
          <line
            x1={margin.left}
            x2={margin.left + plotW}
            y1={y(allowance)}
            y2={y(allowance)}
            stroke="hsl(var(--foreground))"
            strokeOpacity={0.55}
          />
          <text
            x={margin.left + plotW + 6}
            y={y(allowance)}
            dy="0.32em"
            fontSize={10.5}
            fill="hsl(var(--foreground))"
            fillOpacity={0.75}
          >
            {allowanceLabel} {format(allowance)}
          </text>
          <text
            x={margin.left + 6}
            y={margin.top + 12}
            fontSize={10.5}
            fill="hsl(var(--foreground))"
            fillOpacity={0.6}
          >
            {overLabel}
          </text>
          <path d={areaPath} fill="var(--src-app)" opacity={0.12} />
          <path
            d={actualPath}
            fill="none"
            stroke="var(--src-app)"
            strokeWidth={2}
            strokeLinejoin="round"
          />
          {lastIdx < totalDays - 1 && (
            <path
              d={`M${x(lastIdx)},${y(today)} L${x(totalDays - 1)},${y(projected)}`}
              fill="none"
              stroke="var(--src-app)"
              strokeWidth={2}
              strokeDasharray="4 4"
              opacity={0.8}
            />
          )}
          {crossIdx !== null && (
            <circle
              cx={x(crossIdx)}
              cy={y(allowance)}
              r={4}
              fill="var(--viz-ring)"
              stroke="hsl(var(--foreground))"
              strokeWidth={1.5}
            />
          )}
          <circle
            cx={x(lastIdx)}
            cy={y(today)}
            r={4.5}
            fill="var(--src-app)"
            stroke="var(--viz-ring)"
            strokeWidth={2}
          />
          <text
            x={lastIdx === 0 ? x(lastIdx) + 8 : x(lastIdx) - 8}
            y={y(today) - 10}
            textAnchor={lastIdx === 0 ? "start" : "end"}
            fontSize={11}
            fontWeight={600}
            fill="hsl(var(--foreground))"
          >
            Today · {format(today)}
          </text>
          {lastIdx < totalDays - 1 && (
            <text
              x={margin.left + plotW + 6}
              y={y(projected)}
              dy="0.32em"
              fontSize={10.5}
              fill="var(--viz-muted)"
            >
              ~{format(projected)}
            </text>
          )}
          {ticks.map((t) => (
            <text
              key={t.index}
              x={x(t.index)}
              y={margin.top + plotH + 15}
              textAnchor="middle"
              fontSize={10.5}
              fill="var(--viz-muted)"
            >
              {t.label}
            </text>
          ))}
        </svg>
      )}
    </div>
  );
}
