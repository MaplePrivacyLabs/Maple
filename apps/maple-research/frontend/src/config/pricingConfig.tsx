import { Check, X } from "lucide-react";

export type PlanFeature = {
  text: string;
  included: boolean;
  icon?: React.ReactNode;
};

export type PricingPlan = {
  name: string;
  price: string;
  description: string;
  features: PlanFeature[];
  ctaText: string;
  popular?: boolean;
};

export const PRICING_PLANS: PricingPlan[] = [
  {
    name: "Free",
    price: "$0",
    description: "Try Maple with limited usage",
    features: [
      {
        text: "25 messages per week",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "End-to-end encryption",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Access to core AI features",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Search Chat History",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Rename Chats",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      { text: "Image Upload", included: false, icon: <X className="w-4 h-4 text-maple-error" /> },
      {
        text: "Document Upload (PDF, DOC, DOCX, TXT, MD)",
        included: false,
        icon: <X className="w-4 h-4 text-maple-error" />
      },
      {
        text: "Voice Recording (Whisper)",
        included: false,
        icon: <X className="w-4 h-4 text-maple-error" />
      },
      {
        text: "Paid AI models including Gemma 4 31B, GLM 5.3, and Kimi K3",
        included: false,
        icon: <X className="w-4 h-4 text-maple-error" />
      },
      { text: "API Access", included: false, icon: <X className="w-4 h-4 text-maple-error" /> }
    ],
    ctaText: "Start Free"
  },
  {
    name: "Pro",
    price: "$20",
    description: "For power users who need more",
    features: [
      {
        text: "All features from Free",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Generous usage for power users",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Image Upload",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Document Upload (PDF, DOC, DOCX, TXT, MD)",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Voice Recording (Whisper Large v3)",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "All AI models including GLM 5.3 and Kimi K3",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "API Access (use plan credits via API)",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      }
    ],
    ctaText: "Start Chatting",
    popular: true
  },
  {
    name: "Max",
    price: "$100",
    description: "Maximum usage for power users",
    features: [
      {
        text: "All features from Pro",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "200,000 monthly credits (10x Pro)",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Priority support",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Early Access to features and models",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Image Upload",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Document Upload (PDF, DOC, DOCX, TXT, MD)",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Voice Recording (Whisper Large v3)",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "All AI models including GLM 5.3 and Kimi K3",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "API Access (use plan credits via API)",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      }
    ],
    ctaText: "Start Chatting"
  },
  {
    name: "Team",
    price: "$30",
    description: "For teams and businesses",
    features: [
      {
        text: "All features from Pro",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "2x more usage than Pro per team member",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Priority support",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Unified billing",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Pool chat credits among team",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Early Access to features and models",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Image Upload",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Document Upload (PDF, DOC, DOCX, TXT, MD)",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "Voice Recording (Whisper Large v3)",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "All AI models including GLM 5.3 and Kimi K3",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      },
      {
        text: "API Access (use plan credits via API)",
        included: true,
        icon: <Check className="w-4 h-4 text-maple-success" />
      }
    ],
    ctaText: "Start Chatting"
  }
];
