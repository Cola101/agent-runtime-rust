/// What this file is for.
///
/// `mcp.input.required` used to be a dead end: the run said 已挂起 and there was
/// no way to answer it. Answering has a contract the runtime enforces — echo
/// the input id, the response-binding version and the binding digest from the
/// event, answer the exact pending key set, and send values whose JSON types
/// match the schema — and every test here presses the buttons a person would
/// press and asserts what actually left the client.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../App";
import { installFakeRuntime, RUN_INPUT } from "./fake-runtime";

const INPUT_ID = "01a0122e-4c11-7b90-9d63-1f8ac4b57e21";
const BINDING = "7c9f1f5b0f5d4a2e8b6c3d1a9e7f2b4c6d8e0a2c4e6f8a0b2d4f6a8c0e2f4b6d";

/// Opens the queue and returns the card of the run parked on MCP input.
///
/// Scoped to that card on purpose: the queue also holds the approval, and an
/// assertion that matched text anywhere on the page has passed here before
/// while the thing it was about was broken.
async function openQueue(mcpRequests?: Record<string, unknown>) {
  const user = userEvent.setup();
  const bridge = installFakeRuntime(mcpRequests ? { mcpRequests } : {});
  render(<App />);
  // The store polls; the first page has to land before the rail has a queue.
  await waitFor(() => expect(screen.getByRole("button", { name: /对话/ })).toBeTruthy());
  await user.click(
    screen.getAllByRole("button", { name: /^待决定/ }).find((node) => node.classList.contains("r"))!,
  );
  const heading = await screen.findByText("等你回答");
  const card = heading.closest(".gate") as HTMLElement;
  return { user, bridge, card };
}

function actions(card: HTMLElement, key: string) {
  return within(within(card).getByRole("group", { name: new RegExp(`怎么回答 ${key}`) }));
}

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

describe("the 待决定 queue can answer an MCP input request", () => {
  it("sends the identity from the event and one response per pending key", async () => {
    const { user, bridge, card } = await openQueue();

    // The server's own question, and the field it declared for the answer.
    expect(within(card).getByText("Confirm this search")).toBeTruthy();
    await user.click(
      within(within(card).getByRole("group", { name: /^confirmed/ }))
        .getByRole("radio", { name: /是/ }),
    );
    await user.type(within(card).getByRole("textbox", { name: /Note/ }), "checked with ops");
    await user.click(actions(card, "confirmation").getByRole("radio", { name: /接受/ }));
    await user.click(actions(card, "verification").getByRole("radio", { name: /接受/ }));
    await user.click(within(card).getByRole("button", { name: "提交回答" }));

    await waitFor(() =>
      expect(bridge.resolveMcpInput).toHaveBeenCalledWith({
        runId: RUN_INPUT,
        inputId: INPUT_ID,
        inputVersion: 1,
        bindingDigest: BINDING,
        responses: {
          // A boolean sent as a boolean: the runtime compares the value's JSON
          // type against the schema and refuses the whole resolution on a
          // mismatch, so "true" would fail as an answer.
          confirmation: {
            action: "accept",
            content: { confirmed: true, note: "checked with ops" },
          },
          // URL mode accepts with no content; the runtime rejects content here.
          verification: { action: "accept" },
        },
      }));
  });

  it("will not submit until every pending request has an answer", async () => {
    const { user, card } = await openQueue();
    const submit = within(card).getByRole("button", { name: "提交回答" });
    expect(submit.hasAttribute("disabled")).toBe(true);

    await user.click(actions(card, "verification").getByRole("radio", { name: /拒绝/ }));
    // One of two answered is not an answer: the runtime refuses a resolution
    // that does not cover the exact pending set, and a client that sent it
    // would be reporting a decision the person never made about the other.
    expect(submit.hasAttribute("disabled")).toBe(true);

    await user.click(actions(card, "confirmation").getByRole("radio", { name: /拒绝/ }));
    expect(submit.hasAttribute("disabled")).toBe(false);
  });

  it("cannot accept before a required field is answered, and never fills one in", async () => {
    const { user, bridge, card } = await openQueue();
    const accept = actions(card, "confirmation").getByRole("radio", { name: /接受/ });
    expect(accept.hasAttribute("disabled")).toBe(true);
    expect(within(card).getByText(/confirmed 是必填的/)).toBeTruthy();

    await user.click(
      within(within(card).getByRole("group", { name: /^confirmed/ }))
        .getByRole("radio", { name: /否/ }),
    );
    expect(accept.hasAttribute("disabled")).toBe(false);
    await user.click(accept);
    await user.click(actions(card, "verification").getByRole("radio", { name: /取消/ }));
    await user.click(within(card).getByRole("button", { name: "提交回答" }));

    await waitFor(() => expect(bridge.resolveMcpInput).toHaveBeenCalled());
    const sent = bridge.resolveMcpInput.mock.calls[0][0];
    // `false` because it was chosen, and no `note` at all because nobody typed
    // one. An optional field the person left alone is left out, not sent empty.
    expect(sent.responses.confirmation.content).toEqual({ confirmed: false });
    expect(sent.responses.verification).toEqual({ action: "cancel" });
  });

  it("says it does not understand a mode this build does not know", async () => {
    const { user, bridge, card } = await openQueue({
      consent: { mode: "device_consent", message: "Approve on the paired device" },
    });
    expect(within(card).getByText("Approve on the paired device")).toBeTruthy();
    expect(within(card).getByText(/本版本不认识这种请求方式/)).toBeTruthy();

    const group = actions(card, "consent");
    expect(group.getByRole("radio", { name: /接受/ }).hasAttribute("disabled")).toBe(true);
    await user.click(group.getByRole("radio", { name: /拒绝/ }));
    await user.click(within(card).getByRole("button", { name: "提交回答" }));
    await waitFor(() =>
      expect(bridge.resolveMcpInput.mock.calls[0][0]).toMatchObject({
        responses: { consent: { action: "decline" } },
      }));
  });
});

describe("the transcript asks the same question", () => {
  it("renders the URL request with the address the server sent", async () => {
    const { user } = await openQueue();
    await user.click(screen.getByText("Confirm this search"));
    await user.click(
      screen.getAllByRole("button", { name: /^对话/ }).find((n) => n.classList.contains("r"))!,
    );
    const gate = (await screen.findByText("等你回答")).closest(".gate") as HTMLElement;
    expect(within(gate).getByText(/MCP docs/)).toBeTruthy();

    // The address itself, not a word standing in for it, and the id the server
    // will match the completed elicitation by.
    const link = within(gate).getByRole("link", { name: "https://docs.example.test/verify/9f2" });
    expect(link.getAttribute("href")).toBe("https://docs.example.test/verify/9f2");
    expect(within(gate).getByText(/elicit-9f2/)).toBeTruthy();
    // What the answer will be bound to, where a person can read it before
    // pressing anything.
    expect(within(gate).getByText(new RegExp(BINDING.slice(0, 16)))).toBeTruthy();
  });

  it("does not also draw the question as a line in the column", async () => {
    const { user } = await openQueue();
    await user.click(screen.getByText("Confirm this search"));
    await user.click(
      screen.getAllByRole("button", { name: /^对话/ }).find((n) => n.classList.contains("r"))!,
    );
    await screen.findByText("等你回答");
    // The gate below carries the whole question. A hairline in the transcript
    // saying it was asked would be the same fact twice -- which is what the
    // column looked like before `approval.required` was excluded for exactly
    // this reason.
    expect(document.querySelectorAll(".note").length).toBe(0);
    expect(screen.queryByText("MCP 服务要你回答")).toBeNull();
  });
});
