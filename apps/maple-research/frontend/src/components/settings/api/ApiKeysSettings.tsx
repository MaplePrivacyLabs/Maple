import { useState } from "react";
import { Link, useBlocker } from "@tanstack/react-router";
import { useOpenSecret } from "@mapleai/sdk";
import { AlertCircle, Calendar, Clock, KeyRound, Loader2, Plus, Trash2 } from "lucide-react";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { useSettingsNavigationLock } from "@/contexts/SettingsNavigationLockContext";
import { SettingsSection } from "../SettingsPage";
import { useApiKeys } from "./useApiKeys";

const API_KEY_DATE_FORMATTER = new Intl.DateTimeFormat("en-US", {
  month: "short",
  day: "numeric",
  year: "numeric",
  hour: "2-digit",
  minute: "2-digit"
});

function formatDate(dateString: string) {
  return API_KEY_DATE_FORMATTER.format(new Date(dateString));
}

export function ApiKeysSettings() {
  const { deleteApiKey } = useOpenSecret();
  const { data: apiKeys, isLoading, error, refetch } = useApiKeys();
  const [pendingDelete, setPendingDelete] = useState<string | null>(null);
  const [isDeleting, setIsDeleting] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);

  useBlocker({
    shouldBlockFn: () => isDeleting,
    disabled: !isDeleting,
    enableBeforeUnload: isDeleting
  });
  useSettingsNavigationLock(isDeleting);

  const handleDelete = async () => {
    if (!pendingDelete) return;

    setIsDeleting(true);
    setDeleteError(null);
    try {
      await deleteApiKey(pendingDelete);
      await refetch();
      setPendingDelete(null);
    } catch (deleteFailure) {
      console.error("Failed to delete API key:", deleteFailure);
      setDeleteError("Failed to delete this API key. Please try again.");
    } finally {
      setIsDeleting(false);
    }
  };

  return (
    <SettingsSection
      title="API keys"
      description="Create and revoke keys for programmatic access to Maple."
    >
      <div className="space-y-4">
        <Button asChild size="sm">
          <Link to="/settings/api/keys/new">
            <Plus className="mr-2 h-4 w-4" />
            Create new API key
          </Link>
        </Button>

        {isLoading && (
          <div className="flex items-center gap-2 py-6 text-sm text-muted-foreground">
            <Loader2 className="h-4 w-4 animate-spin" />
            Loading API keys...
          </div>
        )}

        {error && (
          <Alert variant="destructive">
            <AlertCircle className="h-4 w-4" />
            <AlertDescription>Failed to load API keys. Please try again.</AlertDescription>
          </Alert>
        )}

        {!isLoading && !error && apiKeys?.length === 0 && (
          <div className="rounded-lg border border-dashed p-6 text-center">
            <KeyRound className="mx-auto h-5 w-5 text-muted-foreground" />
            <p className="mt-2 text-sm font-medium">No API keys yet</p>
            <p className="mt-1 text-xs text-muted-foreground">
              Create a key when you are ready to connect an application or workflow.
            </p>
          </div>
        )}

        {!!apiKeys?.length && (
          <div className="divide-y rounded-lg border border-border/70">
            {apiKeys.map((apiKey) => {
              const isConfirmingDelete = pendingDelete === apiKey.name;

              return (
                <div key={apiKey.name} className="p-3 sm:p-4">
                  <div className="flex min-w-0 items-center justify-between gap-3">
                    <div className="min-w-0 flex-1">
                      <p className="truncate text-sm font-medium">{apiKey.name}</p>
                      <p className="mt-0.5 flex items-center gap-1 text-xs text-muted-foreground">
                        <Calendar className="h-3 w-3" />
                        Created {formatDate(apiKey.created_at)}
                      </p>
                      <p className="mt-0.5 flex items-center gap-1 text-xs text-muted-foreground">
                        <Clock className="h-3 w-3" />
                        {apiKey.last_used_at
                          ? `Last used ${formatDate(apiKey.last_used_at)}`
                          : "Never used"}
                      </p>
                    </div>
                    {!isConfirmingDelete && (
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon"
                        className="h-8 w-8 shrink-0 text-muted-foreground hover:text-destructive"
                        onClick={() => {
                          setPendingDelete(apiKey.name);
                          setDeleteError(null);
                        }}
                        aria-label={`Delete API key ${apiKey.name}`}
                      >
                        <Trash2 className="h-4 w-4" />
                      </Button>
                    )}
                  </div>

                  {isConfirmingDelete && (
                    <div className="mt-3 rounded-lg border border-destructive/35 bg-destructive/5 p-3">
                      <p className="text-sm font-medium text-destructive">Delete this API key?</p>
                      <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
                        Applications using this key will stop working immediately. This cannot be
                        undone.
                      </p>
                      {deleteError && (
                        <p className="mt-2 text-xs text-destructive">{deleteError}</p>
                      )}
                      <div className="mt-3 flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
                        <Button
                          type="button"
                          variant="outline"
                          size="sm"
                          onClick={() => {
                            setPendingDelete(null);
                            setDeleteError(null);
                          }}
                          disabled={isDeleting}
                        >
                          Cancel
                        </Button>
                        <Button
                          type="button"
                          variant="destructive"
                          size="sm"
                          onClick={handleDelete}
                          disabled={isDeleting}
                        >
                          {isDeleting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                          {isDeleting ? "Deleting..." : "Delete key"}
                        </Button>
                      </div>
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )}

        <div className="space-y-1.5 text-xs text-muted-foreground">
          <p>API keys allow you to integrate Maple into applications and workflows.</p>
          <p>Keep keys secure and never share them publicly. Treat them like passwords.</p>
        </div>
      </div>
    </SettingsSection>
  );
}
