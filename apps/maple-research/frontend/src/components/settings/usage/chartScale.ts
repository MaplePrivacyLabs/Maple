/**
 * A "nice" axis so it reads 0, 5K, 10K… rather than 6.3K. Tries 4 and 5
 * intervals and keeps whichever wastes less headroom; 2.5 steps only read
 * cleanly in quarters (250, 500, 750, 1K), so they are limited to 4 intervals.
 * Steps below 10 can't be 2.5, since ticks must stay whole numbers.
 */
export function niceScale(value: number): { max: number; ticks: number[] } {
  const target = Number.isFinite(value) && value > 0 ? value : 1;
  const options = [4, 5].map((intervals) => {
    const raw = target / intervals;
    const exp = Math.pow(10, Math.floor(Math.log10(raw)));
    const n = raw / exp;
    const allowQuarter = intervals === 4 && exp >= 10;
    // Usage counts are whole numbers, so steps never go below 1.
    const step = Math.max(
      1,
      (n <= 1 ? 1 : n <= 2 ? 2 : allowQuarter && n <= 2.5 ? 2.5 : n <= 5 ? 5 : 10) * exp
    );
    return { step, max: Math.ceil(target / step) * step };
  });
  const { step, max } = options.reduce((a, b) => (b.max < a.max ? b : a));
  const ticks: number[] = [];
  for (let i = 0; i * step <= max + step / 2; i += 1) ticks.push(i * step);
  return { max, ticks };
}
