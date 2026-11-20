import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";
import {
  Shield,
  Lock,
  Eye,
  EyeOff,
  Server,
  Cpu,
  Code,
  CheckCircle,
  Loader2,
  ArrowRight,
  ArrowDown,
  Copy,
  Check,
  Bot
} from "lucide-react";
import { useQuery } from "@tanstack/react-query";
import { ParsedAttestationView, useOpenSecret } from "@mapleai/sdk";
import { TopNav } from "@/components/TopNav";
import { FullPageMain } from "@/components/FullPageMain";
import { MarketingHeader } from "@/components/MarketingHeader";
import { marketingUrl } from "@/config/domains";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogTrigger
} from "@/components/ui/dialog";

const LLMS_FULL_URL = marketingUrl("/llms-full.txt");
const LLMS_FULL_PROMPT =
  "Read maple.ai/llms-full.txt and give me a summary of Maple's security architecture. How does their encryption work, what are the trust assumptions, and how does it compare to using ChatGPT or Claude directly?";

function MatchIndicator({ isMatch, text = "It's a match!" }: { isMatch: boolean; text?: string }) {
  return (
    <p className={`text-sm my-2 ${isMatch ? "text-green-500" : "text-red-500"}`}>
      {isMatch ? "✓" : "✗"} {text}
    </p>
  );
}

export const Route = createFileRoute("/proof")({
  component: Verify
});

function ProofDisplay({
  parsedDocument,
  os
}: {
  parsedDocument: ParsedAttestationView;
  os: ReturnType<typeof useOpenSecret>;
}) {
  return (
    <div className="flex flex-col gap-4 text-foreground pt-8">
      <div className="flex flex-col gap-6 dark:border-white/10 border-[hsl(var(--marketing-card-border))] dark:bg-black/75 bg-[hsl(var(--marketing-card))]/80 dark:text-white p-8 border rounded-lg">
        <h3 className="text-2xl font-medium">Server PCR0 Fingerprint</h3>

        {parsedDocument.pcrs.map(
          (pcr) =>
            pcr.id === 0 && (
              <div key={pcr.id}>
                <p>
                  <span className="font-mono break-all">{pcr.value}</span>
                </p>

                <MatchIndicator
                  isMatch={parsedDocument.validatedPcr0Hash?.isMatch ?? false}
                  text={parsedDocument.validatedPcr0Hash?.text ?? ""}
                />
              </div>
            )
        )}
        <p className="text-sm dark:text-white/70 text-foreground/70">
          For technical details, check out the{" "}
          <a
            href="https://docs.aws.amazon.com/enclaves/latest/user/verify-root.html"
            target="_blank"
            rel="noopener noreferrer"
            className="underline hover:text-foreground/80 dark:hover:text-white/80"
          >
            AWS Nitro Enclaves documentation
          </a>
          .
        </p>
      </div>

      <details className="group rounded p-8 dark:bg-black/75 bg-[hsl(var(--marketing-card))]/80 border border-[hsl(var(--marketing-card-border))] dark:border-white/10">
        <summary className="font-medium cursor-pointer hover:text-foreground/80 dark:hover:text-white/80">
          <span className="group-open:hidden">Show</span>
          <span className="hidden group-open:inline">Hide</span>
          {" Full Attestation Details"}
        </summary>
        <div className="mt-4 space-y-4">
          <div className="text-sm dark:text-gray-400 text-foreground/70 space-y-2">
            <p>
              Module ID: <span className="font-mono break-all">{parsedDocument.moduleId}</span>
            </p>
            <p>Timestamp: {parsedDocument.timestamp}</p>
            {parsedDocument.nonce && (
              <p>
                Nonce: <span className="font-mono break-all">{parsedDocument.nonce}</span>
              </p>
            )}
            <p>Digest: {parsedDocument.digest}</p>
            {parsedDocument.publicKey && (
              <>
                <p>
                  Public Key:{" "}
                  <span className="font-mono break-all">{parsedDocument.publicKey}</span>
                </p>
                <MatchIndicator
                  isMatch={true}
                  text="Document signature verified with this public key"
                />
              </>
            )}
            <div>
              <details className="group mt-2">
                <summary className="text-sm cursor-pointer hover:text-foreground/80 dark:hover:text-white/80">
                  Additional PCR Values
                </summary>
                <div className="mt-2 space-y-2">
                  {parsedDocument.pcrs.map(
                    (pcr) =>
                      pcr.id !== 0 && (
                        <div key={pcr.id}>
                          <p>
                            PCR{pcr.id}: <span className="font-mono break-all">{pcr.value}</span>
                          </p>
                        </div>
                      )
                  )}
                </div>
              </details>
            </div>
            <div>
              <p className="font-medium mt-4 mb-2">Certificate Chain:</p>
              {parsedDocument.certificates.map((cert, index) => (
                <div key={index} className="mb-4 p-3 dark:bg-black/50 bg-foreground/5 rounded">
                  {cert.isRoot ? (
                    <p className="font-medium mb-1">Root Certificate</p>
                  ) : (
                    <p className="font-medium mb-1">Certificate {index + 1}</p>
                  )}
                  <p className="text-sm">Subject: {cert.subject}</p>
                  <p className="text-sm">Valid From: {cert.notBefore}</p>
                  <p className="text-sm">Valid Until: {cert.notAfter}</p>
                  {cert.isRoot ? (
                    <>
                      <p>
                        Calculated SHA-256:{" "}
                        <span className="font-mono break-all">{parsedDocument.cert0hash}</span>
                      </p>
                      <p>
                        Expected root cert hash:{" "}
                        <span className="font-mono break-all">{os.expectedRootCertHash}</span>
                      </p>
                      <MatchIndicator
                        isMatch={parsedDocument.cert0hash === os.expectedRootCertHash}
                        text={
                          parsedDocument.cert0hash === os.expectedRootCertHash
                            ? "Root certificate hash matches AWS root certificate"
                            : "Root certificate hash does not match!"
                        }
                      />
                    </>
                  ) : (
                    <MatchIndicator isMatch={true} text="Signature verified with chain" />
                  )}
                  <details className="mt-2">
                    <summary className="text-sm cursor-pointer hover:text-foreground/80 dark:hover:text-white/80">
                      Show PEM Certificate
                    </summary>
                    <pre className="mt-2 p-2 dark:bg-black/50 bg-foreground/5 rounded text-xs font-mono whitespace-pre-wrap break-all">
                      {cert.pem}
                    </pre>
                  </details>
                </div>
              ))}
            </div>
          </div>
        </div>
      </details>
    </div>
  );
}

const DATA_FLOW_STEPS = [
  {
    icon: Lock,
    title: "Encrypted on Device",
    description: "Messages encrypted locally before leaving your device"
  },
  {
    icon: Server,
    title: "Secure Enclave",
    description: "Decrypted only inside a hardware-isolated enclave"
  },
  {
    icon: Cpu,
    title: "GPU TEE",
    description: "AI inference inside a trusted execution environment"
  },
  {
    icon: Code,
    title: "Open Source",
    description: "Verifiable code with reproducible builds"
  }
];

function DataFlowDiagram() {
  return (
    <div className="flex flex-col lg:flex-row items-center lg:items-start gap-4 lg:gap-0 w-full">
      {DATA_FLOW_STEPS.map((step, i) => (
        <div key={step.title} className="contents">
          {/* Step */}
          <div className="flex flex-col items-center text-center gap-3 flex-1 px-2">
            <div className="p-4 rounded-full bg-gradient-to-br from-[hsl(var(--purple))]/20 to-[hsl(var(--purple))]/5 border border-[hsl(var(--purple))]/30">
              <step.icon className="w-7 h-7 text-[hsl(var(--purple))]" />
            </div>
            <h3 className="text-lg font-medium text-foreground">{step.title}</h3>
            <p className="text-sm text-[hsl(var(--marketing-text-muted))] max-w-[200px]">
              {step.description}
            </p>
          </div>
          {/* Arrow between steps */}
          {i < DATA_FLOW_STEPS.length - 1 && (
            <>
              <div className="hidden lg:flex items-center pt-5 px-1 text-[hsl(var(--purple))]/40">
                <ArrowRight className="w-6 h-6" />
              </div>
              <div className="flex lg:hidden items-center py-1 text-[hsl(var(--purple))]/40">
                <ArrowDown className="w-6 h-6" />
              </div>
            </>
          )}
        </div>
      ))}
    </div>
  );
}

const CONTACT_EMAIL = "support@trymaple.ai";
const CONTACT_SUBJECT = "Security Evaluation of Maple";
const CONTACT_BODY = `Hi Maple team,

I'd like to connect you with our security team to discuss Maple's architecture and security model.

Company:
Contact:
`;

function CopyButton({ text, label }: { text: string; label: string }) {
  const [copied, setCopied] = useState(false);
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    return () => {
      if (timeoutRef.current !== null) {
        clearTimeout(timeoutRef.current);
        timeoutRef.current = null;
      }
    };
  }, []);

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      if (timeoutRef.current !== null) {
        clearTimeout(timeoutRef.current);
      }
      timeoutRef.current = setTimeout(() => {
        setCopied(false);
        timeoutRef.current = null;
      }, 2000);
    } catch {
      console.error("Failed to copy to clipboard");
    }
  };

  return (
    <button
      type="button"
      onClick={handleCopy}
      className="inline-flex items-center gap-1.5 px-3 py-1.5 rounded-md text-sm transition-all duration-200
        bg-[hsl(var(--muted))] hover:bg-[hsl(var(--muted))]/80 text-foreground"
    >
      {copied ? (
        <>
          <Check className="w-3.5 h-3.5 text-green-500" />
          Copied
        </>
      ) : (
        <>
          <Copy className="w-3.5 h-3.5" />
          {label}
        </>
      )}
    </button>
  );
}

function ContactButton() {
  const mailto = `mailto:${encodeURIComponent(CONTACT_EMAIL)}?subject=${encodeURIComponent(CONTACT_SUBJECT)}&body=${encodeURIComponent(CONTACT_BODY)}`;

  return (
    <a
      href={mailto}
      className="inline-flex items-center justify-center gap-2 px-8 py-4 rounded-lg text-xl font-light transition-all duration-300
        dark:bg-white/90 dark:text-black dark:hover:bg-[hsl(var(--purple))]/80 dark:hover:text-[hsl(var(--foreground))] dark:active:bg-white/80
        bg-background text-foreground hover:bg-[hsl(var(--purple))] hover:text-[hsl(var(--foreground))] active:bg-background/80
        border border-[hsl(var(--purple))]/30 hover:border-[hsl(var(--purple))]
        shadow-[0_0_15px_rgba(var(--purple-rgb),0.2)] hover:shadow-[0_0_25px_rgba(var(--purple-rgb),0.3)]"
    >
      <Shield className="w-5 h-5" />
      Introduce Us to Your Security Team
    </a>
  );
}

function AgentModal() {
  return (
    <Dialog>
      <DialogTrigger asChild>
        <button
          type="button"
          className="inline-flex items-center justify-center gap-2 px-8 py-4 rounded-lg text-xl font-light transition-all duration-300
            dark:bg-[hsl(var(--background))] dark:border dark:border-[hsl(var(--blue))]/20 dark:text-[hsl(var(--foreground))] dark:hover:border-[hsl(var(--blue))]/80
            bg-[hsl(var(--marketing-card))]
            border border-[hsl(var(--purple))]/20 hover:border-[hsl(var(--purple))]/80
            text-foreground shadow-[0_0_15px_rgba(var(--purple-rgb),0.1)]"
        >
          <Bot className="w-5 h-5" />
          Introduce Us to Your AI Agent
        </button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Bot className="w-5 h-5 text-[hsl(var(--purple))]" />
            Point Your AI Agent at Maple
          </DialogTitle>
          <DialogDescription>
            Our site is ready for AI agents with machine-readable documentation.
          </DialogDescription>
        </DialogHeader>

        <div className="flex flex-col gap-5 mt-2">
          {/* Step 1 */}
          <div className="flex gap-3">
            <div className="flex-shrink-0 w-7 h-7 rounded-full bg-[hsl(var(--purple))]/15 text-[hsl(var(--purple))] flex items-center justify-center text-sm font-medium">
              1
            </div>
            <div className="flex flex-col gap-2 flex-1">
              <p className="text-sm font-medium text-foreground">Give your agent this URL</p>
              <div className="flex items-center justify-between gap-2 px-3 py-2 rounded-md border border-input bg-[hsl(var(--muted))]/50">
                <span className="text-sm font-mono truncate">{LLMS_FULL_URL}</span>
                <CopyButton text={LLMS_FULL_URL} label="Copy" />
              </div>
            </div>
          </div>

          {/* Step 2 */}
          <div className="flex gap-3">
            <div className="flex-shrink-0 w-7 h-7 rounded-full bg-[hsl(var(--purple))]/15 text-[hsl(var(--purple))] flex items-center justify-center text-sm font-medium">
              2
            </div>
            <div className="flex flex-col gap-2 flex-1">
              <p className="text-sm font-medium text-foreground">Ask it something like</p>
              <div className="flex flex-col gap-2">
                <div className="px-3 py-2 rounded-md border border-input bg-[hsl(var(--muted))]/50">
                  <p className="text-sm text-foreground/80 italic">
                    &quot;Read maple.ai/llms-full.txt and give me a summary of Maple&apos;s security
                    architecture. How does their encryption work, what are the trust assumptions,
                    and how does it compare to using ChatGPT or Claude directly?&quot;
                  </p>
                </div>
                <CopyButton text={LLMS_FULL_PROMPT} label="Copy prompt" />
              </div>
            </div>
          </div>

          {/* Step 3 */}
          <div className="flex gap-3">
            <div className="flex-shrink-0 w-7 h-7 rounded-full bg-[hsl(var(--purple))]/15 text-[hsl(var(--purple))] flex items-center justify-center text-sm font-medium">
              3
            </div>
            <div className="flex flex-col gap-1 flex-1">
              <p className="text-sm font-medium text-foreground">
                Works with any web-capable agent
              </p>
              <p className="text-sm text-muted-foreground">
                Claude Code, 🦞 OpenClaw, Codex, OpenCode, Devin, Cursor, Droid, or any agent that
                can fetch URLs.
              </p>
            </div>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function ApproachCard({
  icon: Icon,
  title,
  bullets,
  highlight = false,
  badge
}: {
  icon: React.ElementType;
  title: string;
  bullets: string[];
  highlight?: boolean;
  badge?: string;
}) {
  return (
    <div
      className={`flex flex-col gap-4 p-6 sm:p-8 rounded-xl border transition-all duration-300 relative ${
        highlight
          ? "border-2 border-[hsl(var(--purple))] bg-gradient-to-b from-[hsl(var(--marketing-card))] to-[hsl(var(--marketing-card))]/80 shadow-[0_0_30px_rgba(var(--purple-rgb),0.15)]"
          : "border-[hsl(var(--marketing-card-border))] bg-[hsl(var(--marketing-card))]/50"
      }`}
    >
      {badge && (
        <div className="absolute -top-3 left-1/2 transform -translate-x-1/2 bg-[hsl(var(--purple))] text-[hsl(var(--marketing-card))] px-4 py-1 rounded-full text-sm font-medium whitespace-nowrap">
          {badge}
        </div>
      )}
      <div
        className={`p-3 rounded-full w-fit ${
          highlight
            ? "bg-[hsl(var(--purple))]/20 border border-[hsl(var(--purple))]/40"
            : "bg-[hsl(var(--marketing-card))]/50 border border-[hsl(var(--marketing-card-border))]"
        }`}
      >
        <Icon
          className={`w-6 h-6 ${highlight ? "text-[hsl(var(--purple))]" : "text-foreground/60"}`}
        />
      </div>
      <h3 className="text-xl sm:text-2xl font-medium text-foreground">{title}</h3>
      <ul className="flex flex-col gap-2">
        {bullets.map((bullet, i) => (
          <li key={i} className="flex items-start gap-2 text-[hsl(var(--marketing-text-muted))]">
            <span className="mt-1.5 w-1.5 h-1.5 rounded-full bg-current flex-shrink-0" />
            <span>{bullet}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function SecurityFact({ title, description }: { title: string; description: string }) {
  return (
    <div className="flex items-start gap-3">
      <CheckCircle className="w-5 h-5 text-green-500 mt-0.5 flex-shrink-0" />
      <div>
        <span className="font-medium text-foreground">{title}: </span>
        <span className="text-[hsl(var(--marketing-text-muted))]">{description}</span>
      </div>
    </div>
  );
}

function ProofFAQ() {
  return (
    <div className="flex flex-col gap-8 border-[hsl(var(--marketing-card-border))] bg-[hsl(var(--marketing-card))]/75 text-foreground p-6 sm:p-8 border rounded-lg">
      <h3 className="text-2xl font-medium">FAQ</h3>

      <div className="flex flex-col gap-4">
        <details className="group">
          <summary className="cursor-pointer text-lg font-medium hover:text-foreground/80">
            What is a Trusted Execution Environment (TEE)?
          </summary>
          <p className="mt-4 text-[hsl(var(--marketing-text-muted))]">
            A TEE is a hardware-isolated area of a processor that runs code in a secure enclave.
            Even the server operator cannot access data inside the enclave. AWS Nitro Enclaves,
            which Maple uses, strip away all external access: no SSH, no admin console, no
            persistent storage outside the enclave. The only way in or out is through a narrow,
            measured communication channel.
          </p>
        </details>

        <details className="group">
          <summary className="cursor-pointer text-lg font-medium hover:text-foreground/80">
            Is this the same technology Apple uses for iCloud?
          </summary>
          <p className="mt-4 text-[hsl(var(--marketing-text-muted))]">
            Similar concept, different implementation. Apple&apos;s Private Cloud Compute uses
            custom silicon with Secure Enclave. Maple uses AWS Nitro Enclaves with
            attestation-verified code. Both approaches use hardware isolation to ensure that even
            the service operator cannot access user data during processing.
          </p>
        </details>

        <details className="group">
          <summary className="cursor-pointer text-lg font-medium hover:text-foreground/80">
            How does cross-device sync work if everything is encrypted?
          </summary>
          <p className="mt-4 text-[hsl(var(--marketing-text-muted))]">
            Your account has its own private key derived from your credentials. Chat history is
            encrypted with this key before leaving your device and stored in encrypted form on our
            servers. When you log in on another device, your key is re-derived and used to decrypt
            your data locally.
          </p>
        </details>

        <details className="group">
          <summary className="cursor-pointer text-lg font-medium hover:text-foreground/80">
            Who do I actually have to trust?
          </summary>
          <div className="mt-4 text-[hsl(var(--marketing-text-muted))] space-y-2">
            <p>Your trust assumptions are minimal and verifiable:</p>
            <ul className="list-disc list-inside space-y-1 ml-4">
              <li>
                <strong>Hardware</strong>: AWS Nitro hardware performs as documented (independently
                audited)
              </li>
              <li>
                <strong>Code</strong>: The open-source code running in the enclave does what it says
                (you can audit it)
              </li>
              <li>
                <strong>Attestation</strong>: The cryptographic proof on this page confirms the
                running code matches the published source
              </li>
            </ul>
          </div>
        </details>

        <details className="group">
          <summary className="cursor-pointer text-lg font-medium hover:text-foreground/80">
            Can I verify all of this myself?
          </summary>
          <p className="mt-4 text-[hsl(var(--marketing-text-muted))]">
            Yes. Our{" "}
            <a
              href="https://github.com/MaplePrivacyLabs/Maple/tree/master/services/opensecret"
              target="_blank"
              rel="noopener noreferrer"
              className="text-foreground hover:text-foreground/80 underline"
            >
              server code is open source
            </a>
            . The attestation document on this page is fetched live from our enclave and verified
            against AWS&apos;s root certificate. You can independently reproduce the build, compare
            the PCR0 hash, and confirm that the code running in production matches the published
            source.
          </p>
        </details>
      </div>
    </div>
  );
}

function Verify() {
  const os = useOpenSecret();
  const {
    data: parsedDocument,
    isLoading,
    error
  } = useQuery<ParsedAttestationView>({
    queryKey: ["raw-attestation"],
    queryFn: async () => {
      return await os.getAttestationDocument();
    },
    retry: false
  });

  return (
    <>
      <TopNav />
      <FullPageMain>
        <MarketingHeader
          title={
            <h2 className="text-6xl font-light mb-0">
              Your AI conversations are private.
              <br />
              <span className="dark:text-[hsl(var(--blue))] text-[hsl(var(--purple))]">
                Verify it yourself.
              </span>
            </h2>
          }
          subtitle={
            <p className="text-2xl text-[hsl(var(--marketing-text-muted))] max-w-2xl mx-auto">
              Cryptographic verification, not just promises. Hardware-enforced privacy you can audit
              yourself.
            </p>
          }
        />

        {/* Section 2: How Maple Protects Your Data (flow diagram) */}
        <div className="w-full max-w-7xl mx-auto pt-8">
          <div className="text-center mb-12">
            <h2 className="text-4xl font-light mb-4">
              How Maple{" "}
              <span className="text-[hsl(var(--purple))] font-medium">Protects Your Data</span>
            </h2>
            <p className="text-xl text-[hsl(var(--marketing-text-muted))] max-w-2xl mx-auto">
              Four layers of protection, from your device to the AI model.
            </p>
          </div>

          <DataFlowDiagram />

          <p className="text-sm text-[hsl(var(--marketing-text-muted))] text-center mt-8">
            Both our{" "}
            <a
              href="https://github.com/MaplePrivacyLabs/Maple"
              target="_blank"
              rel="noopener noreferrer"
              className="text-foreground hover:text-foreground/80 underline"
            >
              client
            </a>{" "}
            and{" "}
            <a
              href="https://github.com/MaplePrivacyLabs/Maple/tree/master/services/opensecret"
              target="_blank"
              rel="noopener noreferrer"
              className="text-foreground hover:text-foreground/80 underline"
            >
              server
            </a>{" "}
            code are public. Reproducible builds and attestation let you confirm what&apos;s
            actually running.
          </p>
        </div>

        {/* Section 3: Live Attestation */}
        <div className="w-full max-w-7xl mx-auto pt-8">
          <div className="text-center mb-12">
            <h2 className="text-4xl font-light mb-4">
              Live{" "}
              <span className="dark:text-[hsl(var(--blue))] text-[hsl(var(--purple))] font-medium">
                Cryptographic Attestation
              </span>
            </h2>
            <p className="text-xl text-[hsl(var(--marketing-text-muted))] max-w-2xl mx-auto">
              This attestation document is fetched live from our enclave. Verify it yourself.
            </p>
          </div>

          {isLoading && (
            <div className="flex items-center justify-center p-8">
              <Loader2 className="h-8 w-8 animate-spin text-muted-foreground" />
            </div>
          )}

          {error && (
            <div className="text-red-700 dark:text-red-500 p-4 rounded bg-red-500/10">
              Failed to validate attestation document:{" "}
              {error instanceof Error ? error.message : "Unknown error"}
            </div>
          )}

          {parsedDocument && <ProofDisplay parsedDocument={parsedDocument} os={os} />}

          <p className="text-sm text-[hsl(var(--marketing-text-muted))] text-center mt-8">
            Our code is open source:{" "}
            <a
              href="https://github.com/MaplePrivacyLabs/Maple"
              target="_blank"
              rel="noopener noreferrer"
              className="text-foreground hover:text-foreground/80 underline"
            >
              Maple client
            </a>
            {" and "}
            <a
              href="https://github.com/MaplePrivacyLabs/Maple/tree/master/services/opensecret"
              target="_blank"
              rel="noopener noreferrer"
              className="text-foreground hover:text-foreground/80 underline"
            >
              OpenSecret server
            </a>
          </p>
        </div>

        {/* Section 4: The Spectrum of AI Privacy */}
        <div className="w-full max-w-7xl mx-auto pt-8">
          <div className="text-center mb-12">
            <h2 className="text-4xl font-light mb-4">
              The Spectrum of{" "}
              <span className="text-[hsl(var(--purple))] font-medium">AI Privacy</span>
            </h2>
            <p className="text-xl text-[hsl(var(--marketing-text-muted))] max-w-2xl mx-auto">
              Not all privacy claims are created equal. Here&apos;s how the approaches compare.
            </p>
          </div>

          <div className="grid grid-cols-1 md:grid-cols-3 gap-8">
            <ApproachCard
              icon={Eye}
              title="Standard AI"
              bullets={[
                "Your prompts processed in plaintext on provider servers",
                "Privacy depends on company policies, not technology",
                "Data accessible to employees, subpoenas, and breaches",
                "May be used for model training unless you opt out"
              ]}
            />
            <ApproachCard
              icon={EyeOff}
              title="Privacy Proxy"
              bullets={[
                "A middleman routes your request to strip identifying info",
                "Zero-data-retention is a policy promise, not a guarantee",
                "Your prompts are still processed in plaintext by the AI provider",
                "You must trust the proxy operator and the AI provider"
              ]}
            />
            <ApproachCard
              icon={Shield}
              title="Hardware-Encrypted AI"
              badge="Maple's Approach"
              bullets={[
                "Data encrypted on your device before it leaves",
                "Decrypted only inside a hardware-isolated enclave (TEE)",
                "Your data cannot be used for model training, advertising, or tracking",
                "Open-source code + live attestation so you can verify"
              ]}
              highlight
            />
          </div>
        </div>

        {/* Section 5: For Your Security Team */}
        <div className="w-full max-w-7xl mx-auto pt-8">
          <div className="text-center mb-12">
            <h2 className="text-4xl font-light mb-4">
              For Your <span className="text-[hsl(var(--purple))] font-medium">Security Team</span>
            </h2>
            <p className="text-xl text-[hsl(var(--marketing-text-muted))] max-w-2xl mx-auto">
              Technical facts for evaluating Maple&apos;s security architecture.
            </p>
          </div>

          <div className="p-6 sm:p-8 rounded-xl border border-[hsl(var(--marketing-card-border))] bg-[hsl(var(--marketing-card))]/75">
            <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
              <SecurityFact
                title="Hardware Isolation"
                description="AWS Nitro Enclaves provide CPU-level isolation with no persistent storage, no admin access, and no external networking."
              />
              <SecurityFact
                title="Code Integrity"
                description="Enclave images are measured at boot. The PCR0 hash uniquely identifies the exact code running inside."
              />
              <SecurityFact
                title="Remote Attestation"
                description="A cryptographic attestation document, signed by AWS Nitro hardware, proves the enclave's identity and integrity."
              />
              <SecurityFact
                title="Reproducible Builds"
                description="Anyone can build our open-source code from GitHub and compare the resulting hash against the live attestation."
              />
              <SecurityFact
                title="Minimal Trust Model"
                description="You trust the hardware (AWS Nitro) and the code (open source). No employees, no third parties, no master keys."
              />
              <SecurityFact
                title="Breach Resilience"
                description="Full infrastructure compromise yields only encrypted blobs. Decryption keys exist only inside the TEE and on user devices."
              />
            </div>
            <p className="mt-8 text-sm text-[hsl(var(--marketing-text-muted))]">
              Questions? Reach us at{" "}
              <a
                href="mailto:support@trymaple.ai"
                className="text-foreground hover:text-foreground/80 underline"
              >
                support@trymaple.ai
              </a>
            </p>
          </div>

          <div className="flex flex-col sm:flex-row justify-center items-center gap-4 mt-8">
            <ContactButton />
            <AgentModal />
          </div>
        </div>

        {/* Section 6: FAQ */}
        <div className="w-full max-w-7xl mx-auto pt-8">
          <div className="text-center mb-12">
            <h2 className="text-4xl font-light mb-4">
              Frequently Asked{" "}
              <span className="text-[hsl(var(--purple))] font-medium">Questions</span>
            </h2>
          </div>
          <ProofFAQ />
        </div>
      </FullPageMain>
    </>
  );
}
