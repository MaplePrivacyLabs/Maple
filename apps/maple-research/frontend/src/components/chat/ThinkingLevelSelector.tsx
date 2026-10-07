import { BrainCog, Check } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger
} from "@/components/ui/dropdown-menu";
import { ComposerHoverTooltip } from "@/components/chat/ComposerHoverTooltip";
import type { ModelReasoning } from "@/state/LocalStateContextDef";
import {
  resolveThinkingLevel,
  thinkingLevelLabel,
  type ThinkingLevelChoice
} from "@/services/chatThinkingLevel";

export type ThinkingLevelSelectorProps = {
  /** The selected model's catalog reasoning controls; hides the control when absent. */
  reasoning: ModelReasoning | null | undefined;
  value: ThinkingLevelChoice;
  onChange: (choice: ThinkingLevelChoice) => void;
  disabled?: boolean;
};

/**
 * Composer control for the thinking level sent with each message. The options
 * come from the catalog, so a model only ever receives a level it accepts.
 */
export function ThinkingLevelSelector({
  reasoning,
  value,
  onChange,
  disabled
}: ThinkingLevelSelectorProps) {
  const { options, effective } = resolveThinkingLevel(value, reasoning);
  if (options.length === 0) return null;

  const currentLabel = thinkingLevelLabel(effective);
  const isDefault = effective === "default";

  return (
    <DropdownMenu modal={false}>
      <ComposerHoverTooltip label={`Thinking: ${currentLabel}`}>
        <DropdownMenuTrigger asChild>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            disabled={disabled}
            className="h-8 gap-1 px-2 text-[hsl(var(--maple-secondary-700))] hover:bg-[hsl(var(--maple-primary-container))] hover:text-[hsl(var(--maple-secondary-700))]"
            data-testid="thinking-level-button"
            aria-label={`Thinking level: ${currentLabel}. Click to change.`}
          >
            <BrainCog
              className={`h-4 w-4 ${isDefault ? "" : "text-[hsl(var(--maple-primary))]"}`}
            />
            {!isDefault && <span className="text-xs font-medium">{currentLabel}</span>}
          </Button>
        </DropdownMenuTrigger>
      </ComposerHoverTooltip>
      <DropdownMenuContent align="start" className="w-52">
        {options.map((option) => (
          <DropdownMenuItem
            key={option.value}
            onClick={() => onChange(option.value)}
            className="flex items-center justify-between gap-2"
            data-testid={`thinking-level-option-${option.value}`}
          >
            <span className="flex flex-col">
              <span>{option.label}</span>
              {option.description && (
                <span className="text-xs text-muted-foreground">{option.description}</span>
              )}
            </span>
            {option.value === effective && <Check className="h-4 w-4 shrink-0" />}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
