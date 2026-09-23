import { useCallback, useEffect, useRef, useState, type ReactElement } from "react";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";

type ComposerHoverTooltipProps = {
  children: ReactElement;
  label: string;
  enabled?: boolean;
};

export function ComposerHoverTooltip({
  children,
  label,
  enabled = true
}: ComposerHoverTooltipProps) {
  const [isOpen, setIsOpen] = useState(false);
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const close = useCallback(() => {
    if (timeoutRef.current) {
      clearTimeout(timeoutRef.current);
      timeoutRef.current = null;
    }
    setIsOpen(false);
  }, []);

  useEffect(() => {
    window.addEventListener("blur", close);
    return () => {
      window.removeEventListener("blur", close);
      if (timeoutRef.current) clearTimeout(timeoutRef.current);
    };
  }, [close]);

  useEffect(() => {
    if (!enabled) close();
  }, [enabled, close]);

  return (
    <Tooltip
      open={enabled && isOpen}
      onOpenChange={(open) => {
        if (!open) close();
      }}
      disableHoverableContent
    >
      <TooltipTrigger
        asChild
        onPointerMove={(event) => {
          // Use one hover timer; Radix otherwise starts its own and can skip the delay.
          event.preventDefault();
          if (
            event.pointerType !== "mouse" ||
            event.buttons !== 0 ||
            !enabled ||
            !document.hasFocus() ||
            timeoutRef.current ||
            isOpen
          )
            return;
          timeoutRef.current = setTimeout(() => {
            timeoutRef.current = null;
            setIsOpen(true);
          }, 500);
        }}
        onPointerLeave={close}
        onPointerDown={close}
        onFocus={(event) => event.preventDefault()}
        onBlur={close}
        onClick={close}
      >
        {children}
      </TooltipTrigger>
      <TooltipContent
        side="bottom"
        avoidCollisions={false}
        className="pointer-events-none border-0 bg-[hsl(var(--neutral-900))] text-white"
      >
        {label}
      </TooltipContent>
    </Tooltip>
  );
}
