/// The keys the shell claims on every surface.
///
/// A surface declares its keys as data in `keys:` so the hint on screen comes
/// out of the thing that dispatches it. These had no such declaration: ⌘K and
/// ⌘I were two branches in the shell's key handler and the characters "⌘K"
/// and "⌘I" typed into two pieces of chrome. Three copies of one fact.
/// Declared here for the same reason a surface declares its own, so the
/// reference page can print them without reading the handler and retyping it.
import type { Surface } from "./surfaces/registry";

/// The reference page's id, kept beside the key that opens it. A chord has to
/// have a target, so this is the one surface the shell knows by name; the
/// surface reads the id from here rather than the other way round, because a
/// shell that imports a surface to learn a string is a cycle waiting to
/// happen.
export const KEYS_SURFACE = "keys";

/// What a shell key is allowed to touch: which surface is showing, whether
/// the palette or the drawer is open. That is exactly why these cannot be a
/// surface's `keys:` entries — those act on the Desk, and none of this is on
/// the Desk.
export type Shell = {
  togglePalette(): void;
  toggleDrawer(): void;
  /// Go to a surface, or back to the one before it if it is already showing.
  peek(surfaceId: string): void;
};

export type ShellKey = {
  /// `KeyboardEvent.key` lowercased, matched with ⌘ (or ctrl) held.
  key: string;
  /// The same key as it is printed. The only place this string is written.
  chord: string;
  hint: string;
  run(shell: Shell): void;
  /// The surfaces where the key means something, worked out from the registry
  /// rather than listed by hand. Undefined means everywhere.
  where?(surfaces: readonly Surface[]): string[];
};

export const PALETTE_KEY: ShellKey = {
  key: "k",
  chord: "⌘K",
  hint: "命令面板",
  run: (shell) => shell.togglePalette(),
};

export const DRAWER_KEY: ShellKey = {
  key: "i",
  chord: "⌘I",
  hint: "详情栏",
  where: (surfaces) =>
    surfaces
      .filter((surface) => surface.drawer)
      .map((surface) => `${surface.label}（${surface.drawerLabel ?? "详情"}）`),
  run: (shell) => shell.toggleDrawer(),
};

/// ⌘K and ⌘I are taken, and `?` — the usual key for this — is not available
/// here. A bare key is only dispatched when nothing is being typed, so on the
/// one surface that has a composer `?` would type a question mark instead of
/// opening the reference, which is exactly the moment a person reaches for it.
/// Bare keys also belong to the surfaces: claiming one at shell level would
/// take it away from every surface forever. ⌘/ is `?` with the shift released,
/// it answers while the caret is in a text box, and it costs no letter.
export const REFERENCE_KEY: ShellKey = {
  key: "/",
  chord: "⌘/",
  hint: "键位速查",
  run: (shell) => shell.peek(KEYS_SURFACE),
};

/// Every key the shell claims, in the order the reference prints them.
export const SHELL_KEYS: readonly ShellKey[] = [PALETTE_KEY, DRAWER_KEY, REFERENCE_KEY];

/// Keys with no visible character of their own.
///
/// `KeyboardEvent.key` is what the dispatcher matches, and for nearly every
/// binding it is also what you print. Space is the exception: an empty <kbd>
/// is a hint nobody can read. Kept as a table rather than a branch at one
/// call site because there are now two places that print a binding — the
/// status line and the reference page — and a key that reads one way in one
/// of them and another way in the other is the drift this whole file exists
/// to prevent.
const PRINTED: Record<string, string> = { " ": "空格" };

/// A binding's key as it is shown. Every hint on screen goes through here.
export function printedKey(key: string): string {
  return PRINTED[key] ?? key;
}

/// How a whole binding is written down, modifier included.
///
/// The ⌘ comes off the same declaration the dispatcher matches on, so a hint
/// cannot promise a modifier the dispatcher does not require or drop one it
/// does. And the key itself goes through `printedKey`, because a chord on a key
/// with no visible character has both problems at once.
export function keyLabel(key: { key: string; meta?: boolean }): string {
  const name = printedKey(key.key);
  return key.meta ? `⌘${name.toUpperCase()}` : name;
}
