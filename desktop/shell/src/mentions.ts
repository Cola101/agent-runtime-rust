/// Reading an `@` out of what someone is typing.
///
/// Its own module because the rule is small and easy to get subtly wrong, and
/// because getting it wrong is loud: a file list that opens over an email
/// address interrupts ordinary typing, and one that does not open when it
/// should sends a person back to typing paths from memory.
export type Mention = {
  /// Where the `@` is, so the insertion can replace exactly what was typed.
  at: number;
  /// What has been typed after it, which is what the list narrows by.
  query: string;
};

/// The mention the caret is inside, if it is inside one.
///
/// An `@` counts only at the start of the text or after whitespace. In the
/// middle of a word it is an email address, a decorator or a handle, and none
/// of those is a request for a file list. A space ends it: a mention is one
/// path, and a list that stayed open across the next word would be narrowing
/// by a sentence.
export function mentionAt(text: string, caret: number): Mention | null {
  const before = text.slice(0, caret);
  const at = before.lastIndexOf("@");
  if (at === -1) return null;
  if (at > 0 && !/\s/.test(before[at - 1]!)) return null;
  const query = before.slice(at + 1);
  if (/\s/.test(query)) return null;
  return { at, query };
}

/// The text with the mention replaced by a path, and where the caret goes.
///
/// A trailing space, because a mention is finished once it is chosen and the
/// next thing typed is the rest of the sentence rather than more of the path.
export function withMention(
  text: string,
  mention: Mention,
  path: string,
  caret: number,
): { text: string; caret: number } {
  const inserted = `@${path} `;
  return {
    text: text.slice(0, mention.at) + inserted + text.slice(caret),
    caret: mention.at + inserted.length,
  };
}

/// The entries a query names, narrowest first.
///
/// Case-insensitive and by substring rather than prefix: someone who remembers
/// the middle of a filename is remembering the filename. A name that starts
/// with the query is offered before one that merely contains it, because that
/// is the one they are more likely to have meant.
export function narrow(names: string[], query: string, limit = 8): string[] {
  if (!query) return names.slice(0, limit);
  const wanted = query.toLowerCase();
  const starts: string[] = [];
  const contains: string[] = [];
  for (const name of names) {
    const at = name.toLowerCase().indexOf(wanted);
    if (at === 0) starts.push(name);
    else if (at > 0) contains.push(name);
  }
  return [...starts, ...contains].slice(0, limit);
}
