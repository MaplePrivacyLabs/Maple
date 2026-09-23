import { useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle
} from "@/components/ui/dialog";
import { AlertTriangle } from "lucide-react";
import { Checkbox } from "@/components/ui/checkbox";
import { Label } from "@/components/ui/label";
import { isIOS } from "@/utils/platform";

interface GuestSignupWarningDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onAccept: () => void;
}

export function GuestSignupWarningDialog({
  open,
  onOpenChange,
  onAccept
}: GuestSignupWarningDialogProps) {
  const [bitcoinPaymentAgreed, setBitcoinPaymentAgreed] = useState(false);
  const [noSupportAgreed, setNoSupportAgreed] = useState(false);
  const [backupCredentialsAgreed, setBackupCredentialsAgreed] = useState(false);
  const applePayments = isIOS();

  const allAgreed = bitcoinPaymentAgreed && noSupportAgreed && backupCredentialsAgreed;

  const handleAccept = () => {
    if (allAgreed) {
      // Reset checkboxes for next time
      setBitcoinPaymentAgreed(false);
      setNoSupportAgreed(false);
      setBackupCredentialsAgreed(false);
      onAccept();
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-[calc(100vw-2rem)] sm:max-w-[550px] max-h-[calc(100vh-2rem)] overflow-y-auto [&>button]:hidden">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2 text-maple-warning">
            <AlertTriangle className="w-5 h-5" />
            Anonymous Account - Important Warnings
          </DialogTitle>
          <DialogDescription className="text-base">
            Please read and acknowledge all warnings before creating an anonymous account.
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4 py-4">
          <div className="space-y-4 rounded-lg border border-maple-warning/40 bg-maple-warning/10 p-4">
            {/* Warning 1: Bitcoin Payment */}
            <div className="flex items-start space-x-3">
              <Checkbox
                id="bitcoin-payment"
                checked={bitcoinPaymentAgreed}
                onCheckedChange={(checked: boolean | "indeterminate") =>
                  setBitcoinPaymentAgreed(checked === true)
                }
              />
              <div className="grid gap-1.5 leading-none flex-1">
                <Label
                  htmlFor="bitcoin-payment"
                  className="text-sm font-medium leading-relaxed cursor-pointer break-words"
                >
                  {applePayments ? (
                    <>
                      I understand that Apple subscriptions are linked to this Maple account.{" "}
                      <strong>
                        Restoring an Apple purchase cannot recover a lost Maple Account ID or
                        password.
                      </strong>
                    </>
                  ) : (
                    <>
                      I understand I{" "}
                      <strong>
                        MUST pay for a full year in Bitcoin or redeem a subscription pass
                      </strong>
                      . No credit card, no Stripe, no monthly payment options, and{" "}
                      <strong>no free trial</strong> are available for anonymous accounts.
                    </>
                  )}
                </Label>
              </div>
            </div>

            {/* Warning 2: No Support */}
            <div className="flex items-start space-x-3">
              <Checkbox
                id="no-support"
                checked={noSupportAgreed}
                onCheckedChange={(checked: boolean | "indeterminate") =>
                  setNoSupportAgreed(checked === true)
                }
              />
              <div className="grid gap-1.5 leading-none flex-1">
                <Label
                  htmlFor="no-support"
                  className="text-sm font-medium leading-relaxed cursor-pointer break-words"
                >
                  I understand there is <strong>absolutely no support</strong> available for
                  anonymous accounts. If I have issues, I cannot contact support for help.
                </Label>
              </div>
            </div>

            {/* Warning 3: Backup Credentials */}
            <div className="flex items-start space-x-3">
              <Checkbox
                id="backup-credentials"
                checked={backupCredentialsAgreed}
                onCheckedChange={(checked: boolean | "indeterminate") =>
                  setBackupCredentialsAgreed(checked === true)
                }
              />
              <div className="grid gap-1.5 leading-none flex-1">
                <Label
                  htmlFor="backup-credentials"
                  className="text-sm font-medium leading-relaxed cursor-pointer break-words"
                >
                  I understand I <strong>MUST backup my Account ID</strong> after signup. My Account
                  ID will only be shown once, and if I lose it, I will permanently lose access to my
                  account. We cannot recover it.
                </Label>
              </div>
            </div>
          </div>

          <div className="text-sm text-muted-foreground space-y-2">
            <p>
              <strong>Why these restrictions?</strong>
            </p>
            <p>
              Anonymous accounts are designed for users who prioritize privacy and don't want to
              provide an email address. This comes with trade-offs in support and payment options.
            </p>
          </div>
        </div>

        <DialogFooter className="gap-2 sm:gap-0">
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button onClick={handleAccept} disabled={!allAgreed}>
            I Understand and Accept
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
