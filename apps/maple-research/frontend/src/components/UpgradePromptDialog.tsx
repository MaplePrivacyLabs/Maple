import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import {
  Image,
  Mic,
  Sparkles,
  Check,
  Cpu,
  FileText,
  Gauge,
  MessageCircle,
  Coins,
  Bot,
  AudioLines
} from "lucide-react";
import { useNavigate } from "@tanstack/react-router";
import { useBillingState } from "@/state/useLocalState";
import { hasApiAccess } from "@/billing/billingAccess";
import { isAndroid } from "@/utils/platform";

interface UpgradePromptDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  feature: "image" | "voice" | "tts" | "model" | "document" | "usage" | "tokens" | "agent";
  modelName?: string;
  onStartNewChat?: () => void;
}

export function UpgradePromptDialog({
  open,
  onOpenChange,
  feature,
  modelName,
  onStartNewChat
}: UpgradePromptDialogProps) {
  const navigate = useNavigate();
  const { billingStatus } = useBillingState();

  const handleUpgrade = () => {
    onOpenChange(false);
    navigate({ to: "/pricing" });
  };

  const handleBuyCredits = () => {
    onOpenChange(false);
    navigate({ to: "/settings/api" });
  };

  const handleNewChat = () => {
    onOpenChange(false);
    if (onStartNewChat) {
      onStartNewChat();
      return;
    }
    // Trigger new chat event
    window.dispatchEvent(new Event("newchat"));
    // Clear the URL
    const params = new URLSearchParams(window.location.search);
    params.delete("conversation_id");
    window.history.replaceState({}, "", params.toString() ? `/?${params}` : "/");
  };

  // Determine user's current plan and next upgrade tier
  const currentPlan = billingStatus?.product_name?.toLowerCase() || "free";
  const isFreeTier = !billingStatus?.product_name || currentPlan === "free";
  const isPro = currentPlan.includes("pro") && !currentPlan.includes("max");
  const isMax = currentPlan.includes("max");
  const userHasApiAccess = hasApiAccess(billingStatus);

  const getNextPlan = () => {
    if (isFreeTier) return "Pro";
    if (isPro) return "Max";
    return "Max"; // Already on Max or Team
  };

  const getFeatureInfo = () => {
    if (feature === "image") {
      return {
        icon: <Image className="h-8 w-8" />,
        title: "Image Upload",
        description: "Upload and analyze images with AI-powered vision models",
        requiredPlan: "Pro",
        benefits: [
          "Images stay private with end-to-end encryption",
          "Upload JPEG, PNG, and WebP formats securely",
          "Use Gemma 4 31B and Kimi K2.6 on Pro and above",
          "Analyze diagrams, screenshots, and photos privately",
          "Extract text from images without exposing data"
        ]
      };
    } else if (feature === "voice") {
      return {
        icon: <Mic className="h-8 w-8" />,
        title: "Voice Recording",
        description: "Record and transcribe voice messages with Whisper AI",
        requiredPlan: "Pro",
        benefits: [
          "Voice recordings are end-to-end encrypted",
          "Record messages directly in chat securely",
          "Private transcription with Whisper Large v3",
          "Support for multiple languages",
          "No audio data is stored or used for training"
        ]
      };
    } else if (feature === "tts") {
      return {
        icon: <AudioLines className="h-8 w-8" />,
        title: "Text-to-Speech",
        description: "Listen to Maple's responses with private, natural-sounding speech",
        requiredPlan: "Pro",
        benefits: [
          "Text stays private with end-to-end encryption",
          "Listen to assistant responses on web, mobile, and desktop",
          "Choose from multiple voices and accents",
          "Adjust speech speed in Preferences"
        ]
      };
    } else if (feature === "document") {
      return {
        icon: <FileText className="h-8 w-8" />,
        title: "Document Upload",
        description: "Process and analyze documents with complete privacy",
        requiredPlan: "Pro",
        benefits: [
          "Documents are processed securely with end-to-end encryption",
          "Your files are never stored or used for AI training",
          "Support for PDF, DOC, DOCX, TXT, and Markdown formats",
          "Extract and analyze text while maintaining complete privacy",
          "Local processing ensures your sensitive data never leaves your device"
        ]
      };
    } else if (feature === "usage") {
      const nextPlan = getNextPlan();
      return {
        icon: <Gauge className="h-8 w-8" />,
        title: isFreeTier ? "Daily Usage Limit Reached" : "Monthly Usage Limit Reached",
        description: isFreeTier
          ? "You've reached your daily free tier limit. Upgrade to Pro for unlimited daily usage."
          : isPro
            ? "You've reached your Pro plan's monthly limit. Upgrade to Max for 10x more usage, or purchase extra credits to continue chatting."
            : "You've reached your monthly usage limit. Purchase extra credits to continue chatting, or wait for the next billing cycle.",
        requiredPlan: nextPlan,
        benefits: isFreeTier
          ? [
              "No daily limits with Pro plan",
              "Access to advanced AI models",
              "Priority access during peak times",
              "Higher monthly rate limits for continuous usage",
              "Process images and documents",
              "API access for developers"
            ]
          : isPro
            ? [
                "10x more monthly messages with Max plan",
                "Access to all AI models including GLM 5.3 and Kimi K2.6",
                "Highest priority during peak times",
                "Maximum rate limits for power users",
                "Or purchase extra credits to keep chatting now"
              ]
            : [
                "You're already on our highest individual plan",
                "Purchase extra credits to extend your usage",
                "Monthly usage automatically refreshes",
                "Contact support for custom enterprise plans"
              ]
      };
    } else if (feature === "tokens") {
      return {
        icon: <MessageCircle className="h-8 w-8" />,
        title: "Conversation Limit",
        description:
          "This conversation is too long for the free tier. Upgrade to Pro to continue this chat, or start a new conversation.",
        requiredPlan: "Pro",
        benefits: [
          "No conversation limits",
          "Continue conversations without interruption",
          "Process longer documents and code files",
          "Auto-compaction keeps conversations flowing"
        ]
      };
    } else if (feature === "agent") {
      return {
        icon: <Bot className="h-8 w-8" />,
        title: "Agent Mode",
        description: "Use Maple as a coding agent for multi-step work across local projects",
        requiredPlan: "Pro",
        benefits: [
          "Work through multi-step coding tasks",
          "Read and edit files in projects you choose",
          "Run commands and development tools",
          "Keep separate tasks for each project",
          "Use supported private AI models with API access"
        ]
      };
    } else {
      return {
        icon: <Cpu className="h-8 w-8" />,
        title: modelName ? `Access ${modelName}` : "Powerful AI Models",
        description: "Get access to our most advanced AI models for superior performance",
        requiredPlan: "Pro",
        benefits: [
          "All models run in secure, encrypted environments",
          "Access to GLM 5.3, Kimi K2.6, and the full model lineup",
          "OpenAI GPT-OSS, Gemma, and other advanced models",
          "Higher token limits for longer conversations",
          "Priority access to new models as they launch"
        ]
      };
    }
  };

  const info = getFeatureInfo();

  if (isAndroid()) {
    return (
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>{info.title}</DialogTitle>
            <DialogDescription>
              {feature === "usage"
                ? "You've reached your plan's usage limit. Usage becomes available again when your allowance resets."
                : feature === "tokens"
                  ? "This conversation has reached your plan's limit. You can start a new chat."
                  : `This feature requires a ${info.requiredPlan} plan or above.`}
            </DialogDescription>
          </DialogHeader>
          <p className="text-sm text-muted-foreground">
            Purchases and plan changes are not available in the Android app. You can manage your
            plan on the Maple website and use it here with the same account.
          </p>
          <DialogFooter>
            {feature === "tokens" && (
              <Button variant="outline" onClick={handleNewChat}>
                Start New Chat
              </Button>
            )}
            <Button onClick={() => onOpenChange(false)}>Got it</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    );
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <div className="flex items-center gap-3 mb-2">
            <div className="p-2 rounded-lg bg-primary/10 text-primary">{info.icon}</div>
            <DialogTitle>{info.title}</DialogTitle>
          </div>
          <DialogDescription className="text-base">{info.description}</DialogDescription>
        </DialogHeader>

        <div className="space-y-4 py-4">
          <div className="space-y-2">
            <p className="text-sm font-medium text-muted-foreground">
              {feature === "tts"
                ? "Available on paid Maple plans"
                : info.requiredPlan === "Max" && isMax
                  ? "You're on the Max plan"
                  : `Available with ${info.requiredPlan} plan${info.requiredPlan !== "Max" ? " and above" : ""}`}
            </p>
            <ul className="space-y-2">
              {info.benefits.map((benefit, i) => (
                <li key={i} className="flex items-start gap-2 text-sm">
                  <Check className="mt-0.5 h-4 w-4 shrink-0 text-maple-success" />
                  <span>{benefit}</span>
                </li>
              ))}
            </ul>
          </div>

          {info.requiredPlan !== "Max" || !isMax ? (
            <div className="pt-2 border-t">
              <p className="text-sm text-muted-foreground">
                {isFreeTier
                  ? "Plus access to powerful models, image & document processing, and more"
                  : isPro
                    ? "Plus access to GLM 5.3, Kimi K2.6, 10x more usage, API access, and priority support"
                    : "Explore our pricing options for the best plan for your needs"}
              </p>
            </div>
          ) : null}
        </div>

        <DialogFooter className="flex flex-col gap-2 sm:flex-col sm:justify-stretch sm:space-x-0">
          {(info.requiredPlan !== "Max" || !isMax) && (
            <Button variant="primary" onClick={handleUpgrade} className="w-full gap-2">
              <Sparkles className="h-4 w-4" />
              {isFreeTier ? "Upgrade to Pro" : isPro ? "Upgrade to Max" : "View Plans"}
            </Button>
          )}
          {/* Show Buy Credits button for paid users hitting usage limits */}
          {feature === "usage" && userHasApiAccess && (
            <Button variant="outline" onClick={handleBuyCredits} className="w-full gap-2">
              <Coins className="h-4 w-4" />
              Buy Extra Credits
            </Button>
          )}
          {/* Show "Start New Chat" for free tier conversation limit, "Maybe Later" for others */}
          {feature === "tokens" && isFreeTier ? (
            <Button variant="ghost" onClick={handleNewChat} className="w-full">
              Start New Chat
            </Button>
          ) : (
            <Button variant="ghost" onClick={() => onOpenChange(false)} className="w-full">
              Maybe Later
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
