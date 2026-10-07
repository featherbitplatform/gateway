import { describe, expect, it } from "vitest";
import {
  ALL_METHODS,
  buildMatchRule,
  describeMatch,
  matchRuleToForm,
  parseHostList,
} from "./routeMatch";

describe("parseHostList", () => {
  it("splits on commas, whitespace and newlines, dropping blanks", () => {
    expect(
      parseHostList("a.example.com, *.example.org\n b.example.com  "),
    ).toEqual(["a.example.com", "*.example.org", "b.example.com"]);
  });

  it("lower-cases and de-duplicates", () => {
    expect(parseHostList("API.example.com api.example.com")).toEqual([
      "api.example.com",
    ]);
  });

  it("returns an empty list for empty input", () => {
    expect(parseHostList("")).toEqual([]);
    expect(parseHostList("  \n ")).toEqual([]);
  });
});

describe("buildMatchRule", () => {
  it("omits hosts and methods when they are unconstrained", () => {
    expect(
      buildMatchRule({ path: "/api/*", hosts: "", methods: [...ALL_METHODS] }),
    ).toEqual({
      path: "/api/*",
    });
  });

  it("writes a single host as `hosts` too, never the legacy `host`", () => {
    expect(
      buildMatchRule({ path: "/", hosts: "api.example.com", methods: ["GET"] }),
    ).toEqual({
      path: "/",
      hosts: ["api.example.com"],
      methods: ["GET"],
    });
  });

  it("omits an empty path", () => {
    expect(buildMatchRule({ path: "  ", hosts: "", methods: [] })).toEqual({});
  });

  it("preserves header constraints it does not edit", () => {
    const rule = buildMatchRule(
      { path: "/x", hosts: "", methods: [...ALL_METHODS] },
      { path: "/old", headers: { "x-tier": "gold" }, host: "old.example.com" },
    );
    expect(rule).toEqual({ path: "/x", headers: { "x-tier": "gold" } });
  });
});

describe("matchRuleToForm", () => {
  it("merges legacy host and hosts into one editable list", () => {
    expect(
      matchRuleToForm({
        path: "/api/*",
        host: "legacy.example.com",
        hosts: ["*.example.com"],
      }),
    ).toEqual({
      path: "/api/*",
      hosts: "legacy.example.com, *.example.com",
      methods: [...ALL_METHODS],
    });
  });

  it("treats an absent method list as every method", () => {
    expect(matchRuleToForm({}).methods).toEqual([...ALL_METHODS]);
  });

  it("upper-cases stored methods", () => {
    expect(matchRuleToForm({ methods: ["get", "Post"] }).methods).toEqual([
      "GET",
      "POST",
    ]);
  });
});

describe("describeMatch", () => {
  it("shows just the path when no host is set", () => {
    expect(describeMatch({ path: "/api/*" })).toBe("/api/*");
    expect(describeMatch({})).toBe("/");
  });

  it("prefixes the host when one is set", () => {
    expect(describeMatch({ path: "/api/*", hosts: ["api.example.com"] })).toBe(
      "api.example.com/api/*",
    );
  });

  it("counts extra hosts instead of listing them all", () => {
    expect(
      describeMatch({
        path: "/",
        host: "a.example.com",
        hosts: ["b.example.com", "c.example.com"],
      }),
    ).toBe("a.example.com +2/");
  });
});
