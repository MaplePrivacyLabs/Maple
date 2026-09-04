import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle
} from "@/components/ui/alert-dialog";
import type { NativeOAuthAccount } from "@/services/nativeOAuthAttempt";

interface NativeOAuthAccountConfirmationProps {
  account: NativeOAuthAccount;
  onDecision: (approved: boolean) => void;
}

export function NativeOAuthAccountConfirmation({
  account,
  onDecision
}: NativeOAuthAccountConfirmationProps) {
  return (
    <AlertDialog open onOpenChange={(open) => !open && onDecision(false)}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>Confirm your Maple account</AlertDialogTitle>
          <AlertDialogDescription>
            Check that this is the same account you chose in your browser before signing in.
            <span className="mt-4 block break-all rounded-lg border bg-muted/50 p-3 text-sm font-medium text-foreground">
              {account.email || `Account ${account.userId}`}
            </span>
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel onClick={() => onDecision(false)}>Cancel</AlertDialogCancel>
          <AlertDialogAction onClick={() => onDecision(true)}>Sign in</AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
