import { expect, test } from "bun:test";
import { formatTeamSeatMismatchMessage, type TeamSeatMismatch } from "./teamSeats";

test("team mismatch messages retain recovery without steering iOS admins or members to purchase seats", () => {
  for (const counts of [
    { hasExactCounts: true, memberCount: 4, billedSeatCount: 3, seatsAvailable: 0 },
    { hasExactCounts: false, memberCount: null, billedSeatCount: null, seatsAvailable: null }
  ] satisfies TeamSeatMismatch[]) {
    const admin = formatTeamSeatMismatchMessage(counts, "admin", false);
    const member = formatTeamSeatMismatchMessage(counts, "member", false);
    expect(admin).toContain("Remove members");
    expect(member).toContain("Contact your team admin");
    expect(admin).not.toContain("seats are added");
    expect(member).not.toContain("add paid seats");
    expect(formatTeamSeatMismatchMessage(counts, "admin")).toContain("seats are added");
    expect(formatTeamSeatMismatchMessage(counts, "member")).toContain("add paid seats");
  }
});
