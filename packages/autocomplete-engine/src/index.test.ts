import { describe, expect, it } from "vitest";
import { complete } from "./index.js";

describe("complete", () => {
  it("returns an empty list when the buffer has no command", async () => {
    await expect(complete({ buffer: "" })).resolves.toEqual({
      suggestions: [],
      search_term: "",
    });
  });

  it("returns no suggestions for an unknown command", async () => {
    await expect(
      complete({ buffer: "definitely-not-a-command xyz" }),
    ).resolves.toEqual({
      suggestions: [],
      search_term: "xyz",
    });
  });
});
