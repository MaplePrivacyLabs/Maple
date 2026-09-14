import { useState } from "react";
import { AlertCircle, Check, Copy, Loader2 } from "lucide-react";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { formatModelContext, getModelDisplayName } from "@/services/proxyModels";
import type { OpenSecretModel } from "@/state/LocalStateContextDef";

type ProxyModelListProps = {
  models: OpenSecretModel[];
  isLoading: boolean;
  isError: boolean;
};

function ModelId({ modelId }: { modelId: string }) {
  const [copyState, setCopyState] = useState<"idle" | "copied" | "error">("idle");

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(modelId);
      setCopyState("copied");
      setTimeout(() => setCopyState("idle"), 2000);
    } catch (error) {
      console.error("Failed to copy model ID:", error);
      setCopyState("error");
    }
  };

  return (
    <div className="mt-1 flex min-w-0 items-center gap-1">
      <code className="min-w-0 break-all text-xs text-foreground">{modelId}</code>
      <Button
        type="button"
        variant="ghost"
        size="icon"
        className="h-6 w-6 shrink-0"
        onClick={handleCopy}
        aria-label={
          copyState === "copied"
            ? `Model ID ${modelId} copied`
            : copyState === "error"
              ? `Copy model ID ${modelId} failed`
              : `Copy model ID ${modelId}`
        }
      >
        {copyState === "copied" ? (
          <Check className="h-3 w-3 text-maple-success" />
        ) : copyState === "error" ? (
          <AlertCircle className="h-3 w-3 text-destructive" />
        ) : (
          <Copy className="h-3 w-3" />
        )}
        <span className="sr-only" aria-live="polite">
          {copyState === "copied"
            ? `Model ID ${modelId} copied`
            : copyState === "error"
              ? `Copy model ID ${modelId} failed`
              : ""}
        </span>
      </Button>
    </div>
  );
}

function capabilityLabels(model: OpenSecretModel): string[] {
  const labels: string[] = [];
  if (model.capabilities?.tool_use) labels.push("Tools");
  if (model.capabilities?.vision) labels.push("Vision");
  if (model.capabilities?.tool_use === false) labels.push("No tools");
  return labels;
}

export function ProxyModelList({ models, isLoading, isError }: ProxyModelListProps) {
  return (
    <div className="space-y-4" aria-busy={isLoading}>
      <div>
        <p className="text-sm font-medium">
          {models.length > 0 ? `${models.length} chat model IDs` : "Chat model IDs"}
        </p>
        <p className="mt-1 max-w-2xl text-xs leading-relaxed text-muted-foreground">
          Fetched from Maple&apos;s current client catalog. This confirms configured support, not
          real-time provider uptime or every API-only model.
        </p>
      </div>

      {isLoading && (
        <div
          role="status"
          aria-live="polite"
          className="flex items-center gap-2 rounded-lg border border-dashed p-5 text-sm text-muted-foreground"
        >
          <Loader2 className="h-4 w-4 animate-spin" />
          Loading the current Maple model catalog...
        </div>
      )}

      {isError && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>
            Maple could not load the current model catalog. Restart Maple once you are back online.
          </AlertDescription>
        </Alert>
      )}

      {!isLoading && !isError && models.length === 0 && (
        <div className="rounded-lg border border-dashed p-5 text-sm text-muted-foreground">
          No chat models are currently listed. Restart Maple if this persists.
        </div>
      )}

      {models.length > 0 && (
        <ul className="divide-y overflow-hidden rounded-lg border border-border/70">
          {models.map((model) => {
            const context = formatModelContext(model);

            return (
              <li key={model.id} className="bg-background/40 p-3 sm:p-4">
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <p className="text-sm font-medium">{getModelDisplayName(model)}</p>
                    {(model.badges ?? []).map((badge) => (
                      <Badge key={badge} variant="secondary">
                        {badge}
                      </Badge>
                    ))}
                  </div>
                  <ModelId modelId={model.id} />
                  <div className="mt-1 flex flex-wrap gap-x-3 gap-y-1 text-[11px] text-muted-foreground">
                    {context && <span>{context}</span>}
                    {capabilityLabels(model).map((capability) => (
                      <span key={capability}>{capability}</span>
                    ))}
                  </div>
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
