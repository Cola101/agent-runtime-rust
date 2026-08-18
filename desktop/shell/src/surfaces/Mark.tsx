import { Fragment } from "react";
import { split } from "./find";

/// One run of drawn text, with whatever ⌘F is looking for marked in it.
///
/// `<mark>` is the element for this, and it is also how the finder counts: the
/// marks standing in the column, in document order, are the hits. Everything
/// the transcript draws that a person could search goes through here, and
/// nothing that goes through here is invisible.
export function Mark({ text, query }: { text: string; query: string }) {
  if (!query) return <>{text}</>;
  return (
    <>
      {split(text, query).map((part, index) => (
        part.hit
          ? <mark key={index}>{part.text}</mark>
          : <Fragment key={index}>{part.text}</Fragment>
      ))}
    </>
  );
}

