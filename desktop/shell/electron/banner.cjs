// The words on the banner: the only part of a notification a person reads.
//
// Split out of main.cjs for the reason attention.cjs was: inside main.cjs this
// was two nested ternaries no test could reach, and getting it wrong is not a
// crash — it is an app that tells someone a Run "结果无法判定" when in fact it
// is waiting for their approval, and there is no way to notice that from a
// green suite. Nothing Electron-shaped lives here: main.cjs owns the window
// and the Notification, this owns what they say.
//
// The words are the surface's own. `等你决定` is what the queue writes over an
// approval and `结果无法判定` is what it writes over a Run nobody can judge; a
// banner that used a second wording for the same state would be a second name
// for the same thing. attention.test.js's neighbour holds them equal.

/// A Run id, cut where the rest of this client cuts it (`shortId`). Long
/// enough to tell two Runs apart, short enough to read in a corner of a screen.
function shortRun(runId) {
  return String(runId ?? "").slice(0, 8);
}

/// What one banner says, or null when this build has no words for it.
///
/// Null rather than a blank notification: an interruption that says nothing is
/// worse than not interrupting. A kind this build does not recognise is a
/// window newer than the host, which the host cannot describe honestly.
function bannerText(item) {
  const runId = shortRun(item?.runId);
  switch (item?.kind) {
    case "approval": {
      // An approval whose logged call carried no name is still an approval:
      // the person is still the only way forward, and the Run id is still
      // enough to find it. What it must never do is borrow the other title
      // and claim the result cannot be judged.
      const tool = typeof item.toolName === "string" ? item.toolName : "";
      return {
        title: "等你决定",
        body: tool ? `${tool}・Run ${runId}` : `Run ${runId}`,
      };
    }
    case "indeterminate":
      return { title: "结果无法判定", body: `Run ${runId}・只有你能定` };
    /// More arrived at once than anyone should be handed one at a time. The
    /// count is the point: the ones that did not get their own banner are
    /// still said out loud, as a number, and this lands on the first of them.
    ///
    /// The body does not repeat the title. Read on screen rather than in a
    /// test, "等你决定 / 另外还有 6 个等你决定" says the same three words twice
    /// in a box four words wide.
    case "several":
      return { title: "等你决定", body: `另外还有 ${item.count} 个` };
    default:
      return null;
  }
}

module.exports = { bannerText };
