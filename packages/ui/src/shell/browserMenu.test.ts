import { afterEach, describe, expect, it } from "vitest";
import { suppressBrowserMenu } from "./browserMenu";

function rightClick(target: Element): MouseEvent {
  const e = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });
  target.dispatchEvent(e);
  return e;
}

describe("suppressBrowserMenu", () => {
  let stop: (() => void) | undefined;
  afterEach(() => {
    stop?.();
    stop = undefined;
    document.body.innerHTML = "";
    window.getSelection()?.removeAllRanges();
  });

  it("hides the webview's own menu on plain page content", () => {
    document.body.innerHTML = "<div><p id='p'>Some text</p><button id='b'>Go</button></div>";
    stop = suppressBrowserMenu(document);
    expect(rightClick(document.getElementById("p")!).defaultPrevented).toBe(true);
    expect(rightClick(document.getElementById("b")!).defaultPrevented).toBe(true);
  });

  it("keeps the menu in text fields, where cut, copy and paste make sense", () => {
    document.body.innerHTML =
      "<input id='i' /><textarea id='t'></textarea><div id='c' contenteditable='true'><span id='s'>x</span></div><input id='cb' type='checkbox' />";
    stop = suppressBrowserMenu(document);
    expect(rightClick(document.getElementById("i")!).defaultPrevented).toBe(false);
    expect(rightClick(document.getElementById("t")!).defaultPrevented).toBe(false);
    expect(rightClick(document.getElementById("s")!).defaultPrevented).toBe(false);
    // A checkbox is an input but takes no text.
    expect(rightClick(document.getElementById("cb")!).defaultPrevented).toBe(true);
  });

  it("keeps the menu over selected text, so it can be copied", () => {
    document.body.innerHTML = "<p id='p'>Some selected text</p><p id='other'>Elsewhere</p>";
    stop = suppressBrowserMenu(document);
    const p = document.getElementById("p")!;
    const range = document.createRange();
    range.selectNodeContents(p);
    window.getSelection()!.addRange(range);
    expect(rightClick(p).defaultPrevented).toBe(false);
    // A selection somewhere else does not make the menu useful here.
    expect(rightClick(document.getElementById("other")!).defaultPrevented).toBe(true);
  });

  it("stops when cleaned up", () => {
    document.body.innerHTML = "<p id='p'>Some text</p>";
    suppressBrowserMenu(document)();
    expect(rightClick(document.getElementById("p")!).defaultPrevented).toBe(false);
  });
});
