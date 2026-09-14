import { describe, expect, test } from "bun:test";
import {
  chatCursorAfterSendFailure,
  recoverFailedSendAfterDestinationAdoption
} from "./chatSendFailureRecovery";

describe("chatCursorAfterSendFailure", () => {
  test("rewinds an unconfirmed optimistic cursor to the previous durable item", () => {
    expect(
      chatCursorAfterSendFailure({
        currentCursor: "optimistic-user",
        optimisticMessageId: "optimistic-user",
        previousCursor: "previous-item",
        responseCreated: false
      })
    ).toBe("previous-item");
  });

  test("keeps the persisted user cursor after response.created", () => {
    expect(
      chatCursorAfterSendFailure({
        currentCursor: "optimistic-user",
        optimisticMessageId: "optimistic-user",
        previousCursor: "previous-item",
        responseCreated: true
      })
    ).toBe("optimistic-user");
  });

  test("never rewinds a cursor that already advanced beyond the optimistic user", () => {
    expect(
      chatCursorAfterSendFailure({
        currentCursor: "assistant-item",
        optimisticMessageId: "optimistic-user",
        previousCursor: "previous-item",
        responseCreated: false
      })
    ).toBe("assistant-item");
  });

  test("rewinds an unconfirmed first turn to an empty cursor", () => {
    expect(
      chatCursorAfterSendFailure({
        currentCursor: "optimistic-user",
        optimisticMessageId: "optimistic-user",
        previousCursor: undefined,
        responseCreated: false
      })
    ).toBeUndefined();
  });
});

describe("recoverFailedSendAfterDestinationAdoption", () => {
  test("keeps destination B composer resources while retaining failed source A", () => {
    const destinationImageUrls = new Map([["destination-file", "blob:destination-b"]]);
    const destinationComposer = {
      input: "destination B draft",
      documentText: "destination B document",
      imageUrls: destinationImageUrls,
      imagePasteGeneration: 7,
      documentUploadGeneration: 11
    };
    const destinationHistory = {
      id: "destination-history",
      text: "existing destination history",
      status: "completed"
    };
    const sourceMessage = {
      id: "source-a",
      text: "source A prompt",
      status: "completed"
    };

    const recovery = recoverFailedSendAfterDestinationAdoption(
      true,
      [destinationHistory, sourceMessage],
      destinationComposer,
      sourceMessage.id
    );

    expect(recovery).not.toBeNull();
    expect(recovery?.composer).toBe(destinationComposer);
    expect(recovery?.composer.imageUrls).toBe(destinationImageUrls);
    expect(recovery?.composer).toEqual(destinationComposer);
    expect(recovery?.messages).toEqual([
      destinationHistory,
      { ...sourceMessage, status: "incomplete" }
    ]);
  });

  test("leaves regular sends on the existing origin-composer recovery path", () => {
    expect(
      recoverFailedSendAfterDestinationAdoption(
        false,
        [{ id: "source-a", status: "completed" }],
        { input: "origin" },
        "source-a"
      )
    ).toBeNull();
  });
});
