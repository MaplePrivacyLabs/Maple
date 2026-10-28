import { useEffect, useMemo, useState } from "react";
import {
  AlertCircle,
  Check,
  CheckCircle,
  Copy,
  Loader2,
  Play,
  Server,
  ShieldCheck,
  Square
} from "lucide-react";
import { SettingsSection } from "@/components/settings/SettingsPage";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { proxyService, type ProxyConfig, type ProxyStatus } from "@/services/proxyService";
import { getProxyBaseUrl, isCodingAgentModel } from "@/services/proxyModels";
import type { OpenSecretModel } from "@/state/LocalStateContextDef";
import { isTauriDesktop } from "@/utils/platform";
import { QUICK_MODEL_ALIAS } from "@/utils/utils";
import { ProxyClientGuides } from "./ProxyClientGuides";
import { ProxyModelList } from "./ProxyModelList";

interface ProxyConfigSectionProps {
  apiKeys: Array<{ name: string; created_at: string }>;
  onRequestNewApiKey: (name: string) => Promise<string>;
  models: OpenSecretModel[];
  isModelsLoading: boolean;
  isModelsError: boolean;
}

function isLoopbackHost(host: string): boolean {
  return host.trim() === "127.0.0.1";
}

export function ProxyConfigSection({
  apiKeys,
  onRequestNewApiKey,
  models,
  isModelsLoading,
  isModelsError
}: ProxyConfigSectionProps) {
  const [proxyStatus, setProxyStatus] = useState<ProxyStatus | null>(null);
  const [config, setConfig] = useState<ProxyConfig>({
    host: "127.0.0.1",
    port: 8080,
    api_key: "",
    enabled: false,
    enable_cors: false,
    auto_start: false
  });
  const [isLoading, setIsLoading] = useState(false);
  const [message, setMessage] = useState<{ type: "success" | "error"; text: string } | null>(null);
  const [copied, setCopied] = useState(false);
  const [selectedModelId, setSelectedModelId] = useState("");
  const isTauriDesktopPlatform = isTauriDesktop();

  const guideModels = useMemo(() => models.filter(isCodingAgentModel), [models]);

  useEffect(() => {
    if (guideModels.length === 0) {
      setSelectedModelId("");
      return;
    }

    setSelectedModelId((current) => {
      if (guideModels.some((model) => model.id === current)) return current;
      return guideModels.find((model) => model.id === QUICK_MODEL_ALIAS)?.id ?? guideModels[0].id;
    });
  }, [guideModels]);

  const loadProxyState = async () => {
    try {
      const [savedConfig, status] = await Promise.all([
        proxyService.loadProxyConfig(),
        proxyService.getProxyStatus()
      ]);

      setConfig(savedConfig);
      setProxyStatus(status);
    } catch (error) {
      console.error("Failed to load proxy state:", error);
    }
  };

  useEffect(() => {
    if (!isTauriDesktopPlatform) return;
    void loadProxyState();
  }, [isTauriDesktopPlatform]);

  const handleStartProxy = async () => {
    setIsLoading(true);
    setMessage(null);

    try {
      let apiKey = config.api_key;
      if (!apiKey) {
        const date = new Date().toISOString().split("T")[0].replace(/-/g, "");
        const keyName = `maple-desktop-${date}`;
        const existingKey = apiKeys.find((key) => key.name === keyName);
        if (existingKey) {
          setMessage({
            type: "error",
            text: "Please select an existing API key or create a new one"
          });
          return;
        }

        try {
          apiKey = await onRequestNewApiKey(keyName);
          setConfig((previous) => ({ ...previous, api_key: apiKey }));
        } catch {
          setMessage({
            type: "error",
            text: "Failed to create API key. Please create an API key manually first"
          });
          return;
        }
      }

      const port = Number(config.port);
      if (!Number.isInteger(port) || port < 1 || port > 65535) {
        setMessage({
          type: "error",
          text: `Invalid port: ${config.port}. Port must be between 1 and 65535.`
        });
        return;
      }

      const backendUrl = import.meta.env.VITE_OPEN_SECRET_API_URL || "https://enclave.trymaple.ai";
      const updatedConfig = {
        ...config,
        api_key: apiKey,
        enabled: true,
        backend_url: backendUrl,
        auto_start: config.auto_start
      };

      const status = await proxyService.startManualProxy(updatedConfig);
      setProxyStatus(status);
      setConfig(updatedConfig);
      setMessage({
        type: "success",
        text: `Proxy is now running on ${config.host}:${config.port}`
      });
    } catch (error) {
      setMessage({ type: "error", text: `Failed to start proxy: ${error}` });
    } finally {
      setIsLoading(false);
    }
  };

  const handleStopProxy = async () => {
    setIsLoading(true);
    setMessage(null);

    try {
      const status = await proxyService.stopManualProxy();
      setProxyStatus(status);
      setConfig((previous) => ({ ...previous, enabled: false }));
      setMessage({ type: "success", text: "The proxy has stopped" });
    } catch (error) {
      setMessage({ type: "error", text: `Failed to stop proxy: ${error}` });
    } finally {
      setIsLoading(false);
    }
  };

  const handleConfigChange = (field: keyof ProxyConfig, value: string | number | boolean) => {
    setConfig((previous) => ({ ...previous, [field]: value }));
  };

  const handleAutoStartChange = async (checked: boolean) => {
    const updatedConfig = { ...config, auto_start: checked };
    setConfig(updatedConfig);
    try {
      await proxyService.saveManualProxySettings(updatedConfig);
      setMessage({
        type: "success",
        text: checked ? "Auto-start enabled" : "Auto-start disabled"
      });
    } catch {
      setMessage({ type: "error", text: "Failed to save the auto-start setting" });
    }
  };

  const proxyBaseUrl = getProxyBaseUrl(config.host, config.port);
  const isRunning = proxyStatus?.running || false;

  const copyProxyUrl = async () => {
    try {
      await navigator.clipboard.writeText(proxyBaseUrl);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch (error) {
      console.error("Failed to copy proxy URL:", error);
      setMessage({ type: "error", text: "Failed to copy the proxy URL" });
    }
  };

  if (!isTauriDesktopPlatform) return null;

  return (
    <>
      <SettingsSection
        title="Local OpenAI proxy"
        description="Run a local, OpenAI-compatible Chat Completions endpoint from Maple Desktop."
      >
        <div className="space-y-4">
          {message && (
            <Alert
              className={
                message.type === "error" ? "border-destructive/50" : "border-maple-success/40"
              }
            >
              <div className="flex items-start gap-2">
                {message.type === "error" ? (
                  <AlertCircle className="h-4 w-4 shrink-0" />
                ) : (
                  <CheckCircle className="h-4 w-4 shrink-0 text-maple-success" />
                )}
                <AlertDescription>{message.text}</AlertDescription>
              </div>
            </Alert>
          )}

          <div
            className="flex flex-col gap-4 rounded-xl border border-border/70 bg-background/40 p-4 sm:flex-row sm:items-center sm:justify-between"
            aria-busy={isLoading}
          >
            <div className="flex min-w-0 items-start gap-3">
              <div className="rounded-lg bg-[hsl(var(--maple-primary-container))] p-2 text-[hsl(var(--maple-primary-strong))]">
                <Server className="h-5 w-5" />
              </div>
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-2">
                  <p className="font-medium">{isRunning ? "Proxy running" : "Proxy stopped"}</p>
                  <Badge variant={isRunning ? "secondary" : "outline"}>
                    {isRunning ? "Online" : "Offline"}
                  </Badge>
                </div>
                <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
                  {isRunning
                    ? "Keep Maple open while connected tools are using this endpoint."
                    : "Start the proxy when you are ready to connect an app or coding agent."}
                </p>
              </div>
            </div>
            {!isRunning ? (
              <Button onClick={handleStartProxy} disabled={isLoading} className="shrink-0">
                {isLoading ? (
                  <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                ) : (
                  <Play className="mr-2 h-4 w-4" />
                )}
                {isLoading ? "Starting proxy..." : "Start proxy"}
              </Button>
            ) : (
              <Button
                onClick={handleStopProxy}
                disabled={isLoading}
                variant="destructive"
                className="shrink-0"
              >
                {isLoading ? (
                  <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                ) : (
                  <Square className="mr-2 h-4 w-4" />
                )}
                {isLoading ? "Stopping proxy..." : "Stop proxy"}
              </Button>
            )}
          </div>

          <div>
            <Label htmlFor="proxy-url" className="text-xs">
              OpenAI-compatible base URL
            </Label>
            <div className="mt-1 flex gap-2">
              <Input id="proxy-url" value={proxyBaseUrl} readOnly className="font-mono text-xs" />
              <Button
                type="button"
                size="icon"
                variant="outline"
                onClick={copyProxyUrl}
                aria-label={copied ? "Proxy URL copied" : "Copy proxy URL"}
              >
                {copied ? (
                  <Check className="h-4 w-4 text-maple-success" />
                ) : (
                  <Copy className="h-4 w-4" />
                )}
              </Button>
            </div>
            <p className="mt-1.5 text-xs text-muted-foreground">
              Compatible with clients that use OpenAI Chat Completions.
            </p>
          </div>

          <Alert role="note">
            <ShieldCheck className="h-4 w-4" />
            <AlertDescription>
              {config.enable_cors
                ? "Browser inference never uses Maple's saved proxy credential. Every inference client must supply its own valid Maple API key."
                : "Browser-origin requests are rejected. Local processes can use Maple's saved proxy credential without supplying their own key. Keep the proxy on loopback and run only trusted clients."}
            </AlertDescription>
          </Alert>

          {config.enable_cors && (
            <Alert role="note" className="border-maple-warning/40 bg-maple-warning/10">
              <AlertCircle className="h-4 w-4 text-maple-warning" />
              <AlertDescription>
                CORS is enabled for all browser origins. Each inference request must include its own
                valid Maple API key; the credential saved by Maple Desktop is not used.
              </AlertDescription>
            </Alert>
          )}

          {!isLoopbackHost(config.host) && (
            <Alert role="note" className="border-maple-warning/40 bg-maple-warning/10">
              <AlertCircle className="h-4 w-4 text-maple-warning" />
              <AlertDescription>
                This host may expose the proxy beyond your device. Use 127.0.0.1 unless you fully
                understand the network and billing risk.
              </AlertDescription>
            </Alert>
          )}

          <details className="group rounded-lg border border-border/70">
            <summary className="cursor-pointer select-none px-4 py-3 text-sm font-medium">
              Advanced settings
            </summary>
            <div className="space-y-4 border-t border-border/70 p-4">
              <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
                <div className="grid gap-1.5">
                  <Label htmlFor="proxy-host" className="text-xs">
                    Host
                  </Label>
                  <Input
                    id="proxy-host"
                    value={config.host}
                    onChange={(event) => handleConfigChange("host", event.target.value)}
                    placeholder="127.0.0.1"
                    disabled={isRunning}
                    aria-describedby="proxy-host-description"
                  />
                  <p id="proxy-host-description" className="text-xs text-muted-foreground">
                    Use a numeric IPv4 address. 127.0.0.1 is recommended.
                  </p>
                </div>
                <div className="grid gap-1.5">
                  <Label htmlFor="proxy-port" className="text-xs">
                    Port
                  </Label>
                  <Input
                    id="proxy-port"
                    type="number"
                    value={config.port}
                    onChange={(event) =>
                      handleConfigChange("port", Number.parseInt(event.target.value, 10) || 8080)
                    }
                    placeholder="8080"
                    disabled={isRunning}
                  />
                </div>
              </div>

              <div className="flex items-start justify-between gap-4 border-t border-border/70 pt-4">
                <div>
                  <Label htmlFor="enable-cors" className="text-xs">
                    Enable CORS for browser clients
                  </Label>
                  <p
                    id="enable-cors-description"
                    className="mt-1 text-xs leading-relaxed text-muted-foreground"
                  >
                    Allows browser apps at any origin to call and read from the proxy. Every
                    inference request must include its own valid Maple API key. Desktop clients such
                    as OpenCode do not need CORS.
                  </p>
                </div>
                <Switch
                  id="enable-cors"
                  checked={config.enable_cors ?? false}
                  onCheckedChange={(checked) => handleConfigChange("enable_cors", checked)}
                  disabled={isRunning}
                  aria-describedby="enable-cors-description"
                />
              </div>

              <div className="flex items-start justify-between gap-4 border-t border-border/70 pt-4">
                <div>
                  <Label htmlFor="auto-start" className="text-xs">
                    Auto-start when Maple launches
                  </Label>
                  <p
                    id="auto-start-description"
                    className="mt-1 text-xs leading-relaxed text-muted-foreground"
                  >
                    Available after Maple has created and saved the proxy credential.
                  </p>
                </div>
                <Switch
                  id="auto-start"
                  checked={config.auto_start ?? false}
                  onCheckedChange={handleAutoStartChange}
                  disabled={!config.api_key}
                  aria-describedby="auto-start-description"
                />
              </div>

              {config.api_key && (
                <div className="flex items-center gap-2 text-xs text-muted-foreground">
                  <ShieldCheck className="h-4 w-4 text-maple-success" />
                  Maple has saved its proxy credential.
                </div>
              )}
            </div>
          </details>
        </div>
      </SettingsSection>

      <SettingsSection
        title="Connect your tools"
        description="Follow a built-in setup guide using this proxy URL and Maple's current model catalog."
      >
        <ProxyClientGuides
          baseUrl={proxyBaseUrl}
          isRunning={isRunning}
          hasApiKeys={apiKeys.length > 0}
          models={guideModels}
          selectedModelId={selectedModelId}
          onSelectModel={setSelectedModelId}
        />
      </SettingsSection>

      <SettingsSection
        title="Current Maple models"
        description="Browse current chat model IDs recommended for Local Proxy clients."
      >
        <ProxyModelList models={models} isLoading={isModelsLoading} isError={isModelsError} />
      </SettingsSection>
    </>
  );
}
