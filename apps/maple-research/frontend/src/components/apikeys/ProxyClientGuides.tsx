import { Link } from "@tanstack/react-router";
import { type ReactNode, useState } from "react";
import {
  AlertCircle,
  Check,
  Copy,
  KeyRound,
  Play,
  ShieldCheck,
  TerminalSquare
} from "lucide-react";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue
} from "@/components/ui/select";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  buildOpenCodeLimitConfig,
  buildOpenCodeProviderFields,
  buildCurlProxyExample,
  buildPythonProxyExample,
  getModelDisplayName
} from "@/services/proxyModels";
import type { OpenSecretModel } from "@/state/LocalStateContextDef";

type ProxyClientGuidesProps = {
  baseUrl: string;
  isRunning: boolean;
  hasApiKeys: boolean;
  models: OpenSecretModel[];
  selectedModelId: string;
  onSelectModel: (modelId: string) => void;
};

function CopyButton({ value, label }: { value: string; label: string }) {
  const [copyState, setCopyState] = useState<"idle" | "copied" | "error">("idle");

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(value);
      setCopyState("copied");
      setTimeout(() => setCopyState("idle"), 2000);
    } catch (error) {
      console.error(`Failed to copy ${label}:`, error);
      setCopyState("error");
    }
  };

  return (
    <Button
      type="button"
      variant="ghost"
      size="icon"
      className="h-7 w-7 shrink-0"
      onClick={handleCopy}
      aria-label={
        copyState === "copied"
          ? `${label} copied`
          : copyState === "error"
            ? `Copy ${label} failed`
            : `Copy ${label}`
      }
      title={copyState === "error" ? "Copy failed" : undefined}
    >
      {copyState === "copied" ? (
        <Check className="h-3.5 w-3.5 text-maple-success" />
      ) : copyState === "error" ? (
        <AlertCircle className="h-3.5 w-3.5 text-destructive" />
      ) : (
        <Copy className="h-3.5 w-3.5" />
      )}
      <span className="sr-only" aria-live="polite">
        {copyState === "copied"
          ? `${label} copied`
          : copyState === "error"
            ? `Copy ${label} failed`
            : ""}
      </span>
    </Button>
  );
}

function SetupField({
  label,
  value,
  copyable = true
}: {
  label: string;
  value: string;
  copyable?: boolean;
}) {
  return (
    <div className="min-w-0 rounded-lg border border-border/70 bg-background/60 p-3">
      <p className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
        {label}
      </p>
      <div className="mt-1 flex min-w-0 items-center justify-between gap-2">
        <code className="min-w-0 break-all text-xs text-foreground">{value}</code>
        {copyable && <CopyButton value={value} label={label} />}
      </div>
    </div>
  );
}

function CodeExample({ label, code }: { label: string; code: string }) {
  return (
    <div className="relative overflow-hidden rounded-lg border border-border/70 bg-background">
      <div className="flex items-center justify-between border-b border-border/70 px-3 py-2">
        <span className="text-xs font-medium">{label}</span>
        <CopyButton value={code} label={`${label} example`} />
      </div>
      <pre className="overflow-x-auto p-3 text-xs leading-relaxed">
        <code>{code}</code>
      </pre>
    </div>
  );
}

function Step({ number, children }: { number: number; children: ReactNode }) {
  return (
    <li className="flex gap-3 text-sm leading-relaxed">
      <span
        aria-hidden="true"
        className="mt-0.5 flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-[hsl(var(--maple-primary-container))] text-[11px] font-semibold text-foreground"
      >
        {number}
      </span>
      <span>{children}</span>
    </li>
  );
}

export function ProxyClientGuides({
  baseUrl,
  isRunning,
  hasApiKeys,
  models,
  selectedModelId,
  onSelectModel
}: ProxyClientGuidesProps) {
  const selectedModel = models.find((model) => model.id === selectedModelId);
  const exampleModelId = selectedModel?.id ?? "SELECT_A_MODEL";
  const openCodeFields = buildOpenCodeProviderFields(baseUrl, selectedModel);
  const openCodeLimitConfig = buildOpenCodeLimitConfig(selectedModel);

  return (
    <div className="space-y-4">
      {!isRunning && (
        <Alert role="note">
          <Play className="h-4 w-4" />
          <AlertDescription>
            Start the proxy above before connecting a client. You can prepare the remaining setup
            now.
          </AlertDescription>
        </Alert>
      )}

      <div className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-end">
        <div className="grid gap-2">
          <label htmlFor="proxy-guide-model" className="text-sm font-medium">
            Model used in examples
          </label>
          <Select
            value={selectedModel?.id ?? ""}
            onValueChange={onSelectModel}
            disabled={models.length === 0}
          >
            <SelectTrigger id="proxy-guide-model">
              <SelectValue placeholder="Load the current model catalog" />
            </SelectTrigger>
            <SelectContent>
              {models.map((model) => (
                <SelectItem key={model.id} value={model.id}>
                  {getModelDisplayName(model)} · {model.id}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <Button asChild variant="outline" className="sm:mb-0">
          <Link to="/settings/api/keys/new">
            <KeyRound className="mr-2 h-4 w-4" />
            Create API key
          </Link>
        </Button>
      </div>

      <Tabs defaultValue="opencode" className="w-full">
        <TabsList className="grid h-auto w-full grid-cols-3">
          <TabsTrigger value="opencode">OpenCode</TabsTrigger>
          <TabsTrigger value="curl">cURL</TabsTrigger>
          <TabsTrigger value="python">Python</TabsTrigger>
        </TabsList>

        <TabsContent value="opencode" className="space-y-4 pt-2">
          <ol className="space-y-3">
            <Step number={1}>
              In Maple, create a dedicated API key above and copy it. The full key is shown only
              once. Then return to Local Proxy.
            </Step>
            <Step number={2}>
              In OpenCode, open <strong>Settings → Providers → Custom Provider → Connect</strong>.
            </Step>
            <Step number={3}>Enter the values below, then connect and select the model.</Step>
            {openCodeLimitConfig && (
              <Step number={4}>
                Add the context limit below to your <code>opencode.json</code>. OpenCode cannot read
                it from a custom provider, and needs it to compact a long session before the model
                rejects it.
              </Step>
            )}
          </ol>

          {hasApiKeys && (
            <p className="text-xs leading-relaxed text-muted-foreground">
              Existing keys cannot be revealed again. If you did not save one, create a new key for
              OpenCode.
            </p>
          )}

          <div className="grid gap-2 sm:grid-cols-2">
            <SetupField label="Provider ID" value={openCodeFields.providerId} />
            <SetupField label="Display name" value={openCodeFields.displayName} />
            <SetupField label="Base URL" value={openCodeFields.baseUrl} />
            <SetupField label="API key" value={openCodeFields.apiKey} copyable={false} />
            <SetupField
              label="Model ID"
              value={openCodeFields.modelId}
              copyable={!!selectedModel}
            />
            <SetupField
              label="Model name"
              value={openCodeFields.modelName}
              copyable={!!selectedModel}
            />
            <SetupField label="Headers" value={openCodeFields.headers} copyable={false} />
          </div>

          {openCodeLimitConfig && <CodeExample label="opencode.json" code={openCodeLimitConfig} />}

          <Alert role="note" className="border-maple-warning/40 bg-maple-warning/10">
            <ShieldCheck className="h-4 w-4 text-maple-warning" />
            <AlertDescription className="space-y-1">
              <p>
                Use a real Maple key—never a placeholder. An incoming bearer key overrides the key
                saved by Maple Desktop.
              </p>
              <p>
                When replacing an old key, disconnect and reconnect the provider. OpenCode stores
                credentials separately from project configuration.
              </p>
            </AlertDescription>
          </Alert>
        </TabsContent>

        <TabsContent value="curl" className="space-y-3 pt-2">
          <p className="text-sm leading-relaxed text-muted-foreground">
            Set <code>MAPLE_API_KEY</code> to a real key created in Maple, then run this request.
          </p>
          {selectedModel ? (
            <CodeExample
              label="Streaming Chat Completions"
              code={buildCurlProxyExample(baseUrl, exampleModelId)}
            />
          ) : (
            <div
              role="status"
              className="rounded-lg border border-dashed p-4 text-sm text-muted-foreground"
            >
              Load and select a current model before copying this example.
            </div>
          )}
        </TabsContent>

        <TabsContent value="python" className="space-y-3 pt-2">
          <p className="text-sm leading-relaxed text-muted-foreground">
            Install the OpenAI Python package and set <code>MAPLE_API_KEY</code> to a real Maple
            key.
          </p>
          {selectedModel ? (
            <CodeExample
              label="OpenAI-compatible Python"
              code={buildPythonProxyExample(baseUrl, exampleModelId)}
            />
          ) : (
            <div
              role="status"
              className="rounded-lg border border-dashed p-4 text-sm text-muted-foreground"
            >
              Load and select a current model before copying this example.
            </div>
          )}
        </TabsContent>
      </Tabs>

      <Card className="border-dashed p-3">
        <div className="flex gap-2 text-xs leading-relaxed text-muted-foreground">
          <TerminalSquare className="mt-0.5 h-4 w-4 shrink-0" />
          <p>
            A 401 usually means a missing, invalid, or stale key; a 400 usually means an old model
            ID. A 5xx can be a temporary model-provider outage even when this setup is correct.
          </p>
        </div>
      </Card>
    </div>
  );
}
