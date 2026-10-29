import { createFileRoute } from "@tanstack/react-router";
import { UsageSettings } from "@/components/settings/usage/UsageSettings";

export const Route = createFileRoute("/settings/usage")({
  component: UsageSettings
});
