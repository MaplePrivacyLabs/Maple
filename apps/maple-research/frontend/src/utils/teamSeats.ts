import type { TeamStatus } from "@/types/team";

export type TeamSeatCounts = {
  memberCount: number | null;
  billedSeatCount: number | null;
  seatsAvailable: number | null;
};

export type TeamSeatMismatch = TeamSeatCounts & {
  hasExactCounts: boolean;
};

function toNumber(value: number | undefined): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

export function getTeamSeatCounts(teamStatus?: TeamStatus): TeamSeatCounts {
  const memberCount =
    toNumber(teamStatus?.team_member_count) ??
    toNumber(teamStatus?.members_count) ??
    toNumber(teamStatus?.seats_used);

  const billedSeatCount =
    toNumber(teamStatus?.billed_seat_count) ?? toNumber(teamStatus?.seats_purchased);

  const seatsAvailable = toNumber(teamStatus?.seats_available);

  return {
    memberCount,
    billedSeatCount,
    seatsAvailable: seatsAvailable === null ? null : Math.max(0, seatsAvailable)
  };
}

export function getTeamSeatMismatch(teamStatus?: TeamStatus): TeamSeatMismatch | null {
  if (!teamStatus?.team_created) return null;

  const counts = getTeamSeatCounts(teamStatus);
  const hasExactCounts = counts.memberCount !== null && counts.billedSeatCount !== null;
  const countMismatch =
    hasExactCounts && counts.memberCount !== null && counts.billedSeatCount !== null
      ? counts.memberCount > counts.billedSeatCount
      : false;

  if (!countMismatch && teamStatus.seat_limit_exceeded !== true) return null;

  return {
    ...counts,
    hasExactCounts
  };
}

export function formatTeamSeatMismatchMessage(
  mismatch: TeamSeatMismatch,
  audience: "admin" | "member",
  canPurchaseSeats = true
): string {
  const resolution =
    audience === "admin"
      ? canPurchaseSeats
        ? "Team usage is paused until seats are added or members are removed."
        : "Remove members to match the paid seat count and resume team usage."
      : canPurchaseSeats
        ? "Contact your team admin to add paid seats or remove members."
        : "Contact your team admin to resolve the seat count.";
  if (
    mismatch.hasExactCounts &&
    mismatch.memberCount !== null &&
    mismatch.billedSeatCount !== null &&
    mismatch.memberCount > mismatch.billedSeatCount
  ) {
    const memberLabel = mismatch.memberCount === 1 ? "member" : "members";
    const seatLabel = mismatch.billedSeatCount === 1 ? "paid seat" : "paid seats";
    return (
      `This team has ${mismatch.memberCount} ${memberLabel} but only ` +
      `${mismatch.billedSeatCount} ${seatLabel}. ${resolution}`
    );
  }

  return `This team has more members than paid seats. ${resolution}`;
}
