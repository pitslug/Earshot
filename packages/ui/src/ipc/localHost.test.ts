// The UI's copy of earshot-net's local-address rule against the case table both suites read.

import { describe, expect, it } from "vitest";

import cases from "../../../../fixtures/net/local_host_cases.txt?raw";
import { endpointHost } from "./localHost";

const table = cases
  .split("\n")
  .map((l) => l.trim())
  .filter((l) => l !== "" && !l.startsWith("#"))
  .map((l) => {
    const [url, local, ...rest] = l.split(/\s+/);
    if (rest.length > 0 || (local !== "true" && local !== "false")) throw new Error(`bad case: ${l}`);
    return { url, local: local === "true" };
  });

describe("local endpoint addresses", () => {
  it("has cases", () => {
    expect(table.length).toBeGreaterThan(10);
  });

  it.each(table)("$url is local: $local", ({ url, local }) => {
    expect(endpointHost(url)?.local).toBe(local);
  });

  it("names the host without brackets, and has none for text that is not an address", () => {
    expect(endpointHost("http://[fc00::1]:8080/v1")).toEqual({ host: "fc00::1", local: true });
    expect(endpointHost("https://api.example.com/v1")).toEqual({ host: "api.example.com", local: false });
    expect(endpointHost("")).toBeNull();
    expect(endpointHost("not an address")).toBeNull();
  });

  it("trims Unicode white space as Rust's trim does, but not U+FEFF", () => {
    expect(endpointHost("  http://192.168.1.5/v1 　")).toEqual({ host: "192.168.1.5", local: true });
    expect(endpointHost("﻿http://192.168.1.5/v1")).toBeNull();
  });

  it("knows only http and https addresses, as the app's check does", () => {
    expect(endpointHost("ftp://192.168.1.5/")).toBeNull();
    expect(endpointHost("ws://localhost:8080/")).toBeNull();
    expect(endpointHost("HTTPS://api.example.com/")).toEqual({ host: "api.example.com", local: false });
  });
});
