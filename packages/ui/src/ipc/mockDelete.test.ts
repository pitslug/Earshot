import { describe, expect, it } from "vitest";

import { IpcFailure, type MeetingEvent, MOCK_DELETE_COPY, POST_STEP_MS, createMockIpc } from ".";
import { createManualTimer } from "../time";

// T12.38: the mock follows `deletion::ask` and `deletion::delete`, so the preview and the UI tests
// see what the app does.
describe("mock meeting deletion", () => {
  it("refuses while recording, asks to stop the finishing, then stops it and deletes", async () => {
    const timer = createManualTimer(10_000);
    const ipc = createMockIpc({ timer, meetings: [] });
    const events: MeetingEvent[] = [];
    await ipc.onMeetingEvent((e) => events.push(e));
    const id = await ipc.startMeeting("InRoom", "", false);
    expect(await ipc.meetingDeletion(id)).toBe("Recording");
    await expect(ipc.deleteMeeting(id, true)).rejects.toMatchObject({
      error: { kind: "meeting_recording", message: MOCK_DELETE_COPY.recording },
    });
    await ipc.stopMeeting();
    timer.advance(POST_STEP_MS);
    expect(await ipc.meetingDeletion(id)).toBe("StopsFinishing");
    const refused = await ipc.deleteMeeting(id).catch((e: unknown) => e);
    expect(refused).toBeInstanceOf(IpcFailure);
    expect((refused as IpcFailure).error).toMatchObject({ kind: "meeting_finishing", retryable: true });
    expect((await ipc.listMeetings()).map((m) => m.id)).toEqual([id]);

    await ipc.deleteMeeting(id, true);
    expect(await ipc.listMeetings()).toEqual([]);
    // Its one summary event says the work was stopped, and nothing follows it.
    const summaries = events.filter((e) => e.type === "Summary");
    expect(summaries).toEqual([{ type: "Summary", meeting: id, result: { kind: "PostCancelled" } }]);
    timer.advance(20 * POST_STEP_MS);
    expect(events.filter((e) => e.type === "Summary")).toHaveLength(1);
  });

  it("names a job running on the meeting and deletes nothing", async () => {
    const timer = createManualTimer(10_000);
    const ipc = createMockIpc({ timer });
    const [first] = await ipc.listMeetings();
    await ipc.resummarise(first.id);
    expect(await ipc.meetingDeletion(first.id)).toBe("Resummarising");
    await expect(ipc.deleteMeeting(first.id, true)).rejects.toMatchObject({
      error: { kind: "meeting_job", message: MOCK_DELETE_COPY.resummarising },
    });
  });
});
